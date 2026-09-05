use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use koharu_pipeline::{Committer, Progress, RunStatus, StageOutput, StopToken};
use koharu_scene::Snapshot;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Manager as _, State, ipc::Channel};
use tauri_runtime_cef::CefRuntime;
use uuid::Uuid;

use super::{ChannelExt as _, Error, canvas::CanvasChannel, project::CurrentProject};
use koharu_desktop::Desktop;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, Type)]
#[serde(transparent)]
pub struct JobId(Uuid);

impl JobId {
    #[must_use]
    pub fn new() -> Self {
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

impl std::str::FromStr for JobId {
    type Err = <Uuid as std::str::FromStr>::Err;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct Job {
    pub id: JobId,
    pub state: JobState,
    #[specta(type = f64)]
    pub completed: usize,
    #[specta(type = f64)]
    pub total: usize,
    pub page: Option<koharu_scene::EntityId>,
    pub stage: Option<koharu_pipeline::Stage>,
    pub model: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Finished,
    Failed,
    Stopped,
}

/// How many completed jobs are retained for the API's `GET /v1/jobs/{id}`.
/// The desktop only needs running jobs (terminal states arrive as events);
/// finished jobs are kept so external tools can poll to a terminal state.
const MAX_JOBS: usize = 16;

#[derive(Default)]
pub(crate) struct Processing {
    pub(crate) stops: Mutex<HashMap<JobId, StopToken>>,
    pub(crate) jobs: Mutex<VecDeque<Job>>,
    pub(crate) inpainting_mask: Mutex<Option<koharu_pipeline::InpaintingMask>>,
}

#[derive(Default)]
pub(crate) struct JobChannel {
    pub(crate) channel: Mutex<Option<Channel<Job>>>,
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn process(
    handle: AppHandle<CefRuntime>,
    scope: koharu_pipeline::Scope,
    operation: koharu_pipeline::Operation,
) -> std::result::Result<JobId, Error> {
    Ok(start_process(&handle, scope, operation).await?)
}

#[tracing::instrument(skip_all)]
pub(crate) async fn start_process(
    handle: &AppHandle<CefRuntime>,
    scope: koharu_pipeline::Scope,
    operation: koharu_pipeline::Operation,
) -> Result<JobId> {
    use anyhow::{Context as _, bail};

    let snapshot = handle
        .state::<CurrentProject>()
        .project
        .lock()
        .await
        .as_ref()
        .context("no project is open")?
        .snapshot();
    let id = JobId::new();
    let stop = StopToken::default();
    {
        let processing = handle.state::<Processing>();
        let mut stops = processing.stops.lock();
        if !stops.is_empty() {
            bail!("another process is already running");
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
    {
        let processing = handle.state::<Processing>();
        let mut jobs = processing.jobs.lock();
        // Older finished jobs roll off once the retained history is full. The
        // running job is the most recent entry, so it is never evicted.
        while jobs.len() >= MAX_JOBS {
            jobs.pop_front();
        }
        jobs.push_back(job.clone());
    }
    handle.state::<JobChannel>().channel.publish(job);

    let pipeline = handle.state::<koharu_pipeline::Pipeline>().inner().clone();
    let task_handle = handle.clone();
    let inpainting_mask = handle.state::<Processing>().inpainting_mask.lock().take();
    drop(tokio::spawn(async move {
        let progress = Arc::new(Mutex::new((0_usize, 0_usize)));
        let progress_handle = task_handle.clone();
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
                    tracing::info!(
                        target: "koharu_metrics",
                        metric = "pipeline_start",
                        page_count = pages.len(),
                        stage_count = stages.len(),
                    );
                    let mut progress = progress.lock();
                    *progress = (0, pages.len().saturating_mul(stages.len()));
                    Some((0, progress.1, None, None, None))
                }
                Progress::Loading { page, stage, model } => {
                    tracing::info!(
                        target: "koharu_metrics",
                        metric = "stage_loading",
                        stage = %stage,
                        model,
                    );
                    let progress = progress.lock();
                    Some((progress.0, progress.1, Some(page), Some(stage), Some(model)))
                }
                Progress::Finished {
                    page,
                    stage,
                    model,
                    elapsed,
                } => {
                    if stage != koharu_pipeline::Stage::Translation {
                        tracing::info!(
                            target: "koharu_metrics",
                            metric = "model_run",
                            stage = %stage,
                            model,
                            duration_ms = elapsed.as_secs_f64() * 1000.0,
                        );
                    }
                    let mut progress = progress.lock();
                    progress.0 = progress.0.saturating_add(1).min(progress.1);
                    Some((progress.0, progress.1, Some(page), Some(stage), Some(model)))
                }
                Progress::Skipped { page, stage } => {
                    tracing::info!(
                        target: "koharu_metrics",
                        metric = "stage_skip",
                        stage = %stage,
                    );
                    let mut progress = progress.lock();
                    progress.0 = progress.0.saturating_add(1).min(progress.1);
                    Some((progress.0, progress.1, Some(page), Some(stage), None))
                }
                Progress::Running { stage, model, .. } => {
                    tracing::info!(
                        target: "koharu_metrics",
                        metric = "stage_running",
                        stage = %stage,
                        model,
                    );
                    None
                }
            };
            if let Some((completed, total, page, stage, model)) = update {
                let job = {
                    let processing = progress_handle.state::<Processing>();
                    let mut jobs = processing.jobs.lock();
                    jobs.iter_mut().find(|job| job.id == id).map(|job| {
                        job.completed = completed;
                        job.total = total;
                        job.page = page;
                        job.stage = stage;
                        job.model = model;
                        job.clone()
                    })
                };
                if let Some(job) = job {
                    progress_handle.state::<JobChannel>().channel.publish(job);
                }
            }
        }));

        struct PipelineCommitter {
            handle: AppHandle<CefRuntime>,
        }

        #[async_trait::async_trait]
        impl Committer for PipelineCommitter {
            async fn commit(&mut self, output: StageOutput) -> Result<Snapshot> {
                let (commit, page) = {
                    let projects = self.handle.state::<CurrentProject>();
                    let mut projects = projects.project.lock().await;
                    let project = projects.as_mut().context("no project is open")?;
                    let Some(commit) = project.commit_rebased(output.patch).await? else {
                        return Ok(project.snapshot());
                    };
                    project.record_commit(&commit);
                    let page = project.active_page();
                    (commit, page)
                };
                let snapshot = commit.snapshot.clone();
                let desktop = self.handle.state::<Desktop>();
                desktop.synchronize(&commit.snapshot, page, &commit).await?;
                let canvas = desktop.canvas_state();
                self.handle.state::<CanvasChannel>().channel.publish(canvas);
                Ok(snapshot)
            }
        }

        let mut committer = PipelineCommitter {
            handle: task_handle.clone(),
        };
        let result = pipeline.execute(snapshot, request, &mut committer).await;
        let (stopped, error) = match result {
            Ok(report) => (report.status == RunStatus::Stopped, None),
            Err(error) => {
                tracing::error!(stage = ?error.stage, %error, "processing failed");
                (false, Some(format!("{error:#}")))
            }
        };
        tracing::info!(
            target: "koharu_metrics",
            metric = "pipeline_result",
            outcome = if stopped {
                "stopped"
            } else if error.is_some() {
                "failed"
            } else {
                "completed"
            },
        );
        task_handle.state::<Processing>().stops.lock().remove(&id);
        let job = task_handle
            .state::<Processing>()
            .jobs
            .lock()
            .iter_mut()
            .find(|job| job.id == id)
            .map(|job| {
                job.state = if stopped {
                    JobState::Stopped
                } else if error.is_some() {
                    JobState::Failed
                } else {
                    JobState::Finished
                };
                job.error = error;
                job.clone()
            });
        if let Some(job) = job {
            task_handle.state::<JobChannel>().channel.publish(job);
        }
    }));
    Ok(id)
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "pipeline_stop",
    skip_all,
    fields(state = "requested")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn stop_job(
    job: JobId,
    processing: State<'_, Processing>,
) -> std::result::Result<(), Error> {
    let stops = processing.stops.lock();
    let stop = stops
        .get(&job)
        .with_context(|| format!("job {job} is not running"))?;
    stop.stop();
    Ok(())
}
