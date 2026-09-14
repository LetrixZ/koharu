use std::{collections::HashMap, fmt, sync::Arc};

use anyhow::Result;
use koharu_pipeline::{Committer, Pipeline, Progress, RunStatus, StageOutput, StopToken};
use koharu_scene::Snapshot;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{handlers::StatusError, project::Project};

#[derive(Default)]
pub(crate) struct Processing {
    pub(crate) stops: Mutex<HashMap<JobId, StopToken>>,
    pub(crate) jobs: Mutex<HashMap<JobId, Job>>,
    pub(crate) jobs_channels: Mutex<HashMap<JobId, broadcast::Sender<Job>>>,
    pub(crate) inpainting_mask: Mutex<Option<koharu_pipeline::InpaintingMask>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub id: JobId,
    pub state: JobState,
    pub completed: usize,
    pub total: usize,
    pub page: Option<koharu_scene::EntityId>,
    pub stage: Option<koharu_pipeline::Stage>,
    pub model: Option<String>,
    pub error: Option<String>,
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
pub enum JobState {
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
            let update = match event {
                Progress::Started { pages, stages } => {
                    let mut progress = progress.lock();
                    *progress = (0, pages.len().saturating_mul(stages.len()));
                    Some((0, progress.1, None, None, None))
                }
                Progress::Loading { page, stage, model } => {
                    let progress = progress.lock();
                    Some((progress.0, progress.1, Some(page), Some(stage), Some(model)))
                }
                Progress::Finished {
                    page, stage, model, ..
                } => {
                    if stage != koharu_pipeline::Stage::Translation {}
                    let mut progress = progress.lock();
                    progress.0 = progress.0.saturating_add(1).min(progress.1);
                    Some((progress.0, progress.1, Some(page), Some(stage), Some(model)))
                }
                Progress::Skipped { page, stage } => {
                    let mut progress = progress.lock();
                    progress.0 = progress.0.saturating_add(1).min(progress.1);
                    Some((progress.0, progress.1, Some(page), Some(stage), None))
                }
                Progress::Running { .. } => None,
            };
            if let Some((completed, total, page, stage, model)) = update {
                let mut jobs = progress_processing.jobs.lock();
                if let Some(job) = jobs.get_mut(&progress_id).map(|job| {
                    job.completed = completed;
                    job.total = total;
                    job.page = page;
                    job.stage = stage;
                    job.model = model;
                    job.clone()
                }) {
                    let channels = progress_processing.jobs_channels.lock();
                    channels
                        .get(&progress_id)
                        .map(|sender| _ = sender.send(job));
                }
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
