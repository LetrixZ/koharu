use std::{collections::HashMap, fmt, sync::Arc};

use anyhow::Result;
use koharu_pipeline::{Committer, Pipeline, Progress, RunStatus, StageOutput, StopToken};
use koharu_scene::Snapshot;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    handlers::StatusError,
    project::{Project, StageProgress},
};

#[derive(Default)]
pub(crate) struct Processing {
    pub(crate) stops: Mutex<HashMap<JobId, StopToken>>,
    pub(crate) jobs: Mutex<HashMap<JobId, Job>>,
    pub(crate) jobs_channels: Mutex<HashMap<JobId, broadcast::Sender<Job>>>,
    pub(crate) inpainting_mask: Mutex<Option<koharu_pipeline::InpaintingMask>>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Job {
    pub(crate) id: JobId,
    pub(crate) state: JobState,
    pub(crate) completed: usize,
    pub(crate) total: usize,
    pub(crate) page: Option<koharu_scene::EntityId>,
    pub(crate) stage: Option<koharu_pipeline::Stage>,
    pub(crate) model: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) pages: Vec<PageProgress>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PageProgress {
    pub(crate) page: koharu_scene::EntityId,
    pub(crate) stages: StageProgress,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct JobId(pub(crate) Uuid);

impl JobId {
    #[must_use]
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobState {
    Running,
    Finished,
    Failed,
    Stopped,
}

pub(crate) async fn process(
    project: Project,
    scope: koharu_pipeline::Scope,
    operation: koharu_pipeline::Operation,
    pipeline: Pipeline,
    processing: Arc<Processing>,
) -> Result<JobId> {
    let id = JobId::new();
    let stop = StopToken::default();
    {
        let mut stops = processing.stops.lock();
        if !stops.is_empty() {
            return Err(
                StatusError::BadRequest("another process is already running".to_string()).into(),
            );
        }
        stops.insert(id, stop.clone());
    }
    let job = Job {
        id,
        state: JobState::Running,
        completed: 0,
        total: 0,
        page: None,
        stage: None,
        model: None,
        error: None,
        pages: Vec::new(),
    };
    processing.jobs.lock().insert(id, job.clone());
    let (tx, _) = broadcast::channel::<Job>(16);
    processing.jobs_channels.lock().insert(id, tx);

    let inpainting_mask = processing.inpainting_mask.lock().take();
    drop(tokio::spawn(async move {
        let progress = Arc::new(Mutex::new((0_usize, 0_usize)));
        let progress_processing = processing.clone();
        let progress_id = id;
        let mut request = koharu_pipeline::Request {
            operation,
            scope,
            stop: stop.clone(),
            progress: None,
            inpainting_mask,
        };
        request.progress = Some(Arc::new(move |event| {
            let mut jobs = progress_processing.jobs.lock();
            let Some(job) = jobs.get_mut(&progress_id) else {
                return;
            };

            match event {
                Progress::Started { pages, stages } => {
                    job.pages = pages
                        .iter()
                        .map(|page_id| PageProgress {
                            page: *page_id,
                            stages: StageProgress {
                                detection: false,
                                ocr: false,
                                translation: false,
                                inpainting: false,
                            },
                        })
                        .collect();
                    let mut progress = progress.lock();
                    *progress = (0, pages.len().saturating_mul(stages.len()));
                    job.completed = 0;
                    job.total = progress.1;
                }
                Progress::Loading { page, stage, model } => {
                    job.page = Some(page);
                    job.stage = Some(stage);
                    job.model = Some(model);
                }
                Progress::Finished {
                    page, stage, model, ..
                } => {
                    if let Some(page_progress) = job.pages.iter_mut().find(|p| p.page == page) {
                        match stage {
                            koharu_pipeline::Stage::Detection => {
                                page_progress.stages.detection = true
                            }
                            koharu_pipeline::Stage::Ocr => page_progress.stages.ocr = true,
                            koharu_pipeline::Stage::Translation => {
                                page_progress.stages.translation = true
                            }
                            koharu_pipeline::Stage::Inpainting => {
                                page_progress.stages.inpainting = true
                            }
                        }
                    }
                    let mut progress = progress.lock();
                    progress.0 = progress.0.saturating_add(1).min(progress.1);
                    job.completed = progress.0;
                    job.page = Some(page);
                    job.stage = Some(stage);
                    job.model = Some(model);
                }
                Progress::Skipped { page, stage } => {
                    if let Some(page_progress) = job.pages.iter_mut().find(|p| p.page == page) {
                        match stage {
                            koharu_pipeline::Stage::Detection => {
                                page_progress.stages.detection = true
                            }
                            koharu_pipeline::Stage::Ocr => page_progress.stages.ocr = true,
                            koharu_pipeline::Stage::Translation => {
                                page_progress.stages.translation = true
                            }
                            koharu_pipeline::Stage::Inpainting => {
                                page_progress.stages.inpainting = true
                            }
                        }
                    }
                    let mut progress = progress.lock();
                    progress.0 = progress.0.saturating_add(1).min(progress.1);
                    job.completed = progress.0;
                    job.page = Some(page);
                    job.stage = Some(stage);
                }
                Progress::Running { .. } => {}
            }

            let job = job.clone();
            drop(jobs);
            let channels = progress_processing.jobs_channels.lock();
            if let Some(sender) = channels.get(&progress_id) {
                let _ = sender.send(job);
            }
        }));

        struct PipelineCommitter<'a> {
            project: &'a mut Project,
        }

        #[async_trait::async_trait]
        impl Committer for PipelineCommitter<'_> {
            async fn commit(&mut self, output: StageOutput) -> Result<Snapshot> {
                let commit = {
                    let Some(commit) = self.project.commit_rebased(output.patch).await? else {
                        return Ok(self.project.snapshot());
                    };
                    commit
                };
                let snapshot = commit.snapshot.clone();
                Ok(snapshot)
            }
        }

        let mut project = project;
        let snapshot = project.snapshot();
        let mut committer = PipelineCommitter {
            project: &mut project,
        };
        let result = pipeline.execute(snapshot, request, &mut committer).await;
        let (stopped, error) = match result {
            Ok(report) => (report.status == RunStatus::Stopped, None),
            Err(error) => (false, Some(format!("{error:#}"))),
        };

        processing.stops.lock().remove(&id);
        if let Some(job) = processing.jobs.lock().get_mut(&id) {
            job.state = if stopped {
                JobState::Stopped
            } else if error.is_some() {
                JobState::Failed
            } else {
                JobState::Finished
            };
            job.error = error;

            let mut channels = processing.jobs_channels.lock();
            if let Some(sender) = channels.get(&progress_id) {
                let _ = sender.send(job.clone());
            }
            channels.remove(&progress_id);
        }
    }));

    Ok(id)
}
