use std::{collections::HashMap, convert::Infallible, fmt, sync::Arc};

use anyhow::Result;
use axum::{
    Json,
    extract::{self, Path, State},
    http::StatusCode,
    response::{
        Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::stream::Stream;
use koharu_pipeline::{Committer, Pipeline, Progress, RunStatus, StageOutput, StopToken};
use koharu_scene::Snapshot;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::handlers::StatusError;

use super::{
    ApiResult,
    projects::{Project, ProjectLibrary},
};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Finished,
    Failed,
    Stopped,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
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

#[derive(Debug, Deserialize)]
pub(crate) struct ProcessRequest {
    #[serde(flatten)]
    scope: koharu_pipeline::Scope,
    #[serde(flatten)]
    operation: koharu_pipeline::Operation,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProcessResponse {
    job_id: String,
}

pub(crate) async fn process(
    State(processing): State<Arc<Processing>>,
    State(pipeline): State<Pipeline>,
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
    extract::Json(payload): extract::Json<ProcessRequest>,
) -> ApiResult<Json<ProcessResponse>> {
    let project = library.open(&name).await?;

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
            operation: payload.operation,
            scope: payload.scope,
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
    Ok(Json(ProcessResponse {
        job_id: id.to_string(),
    }))
}

pub(crate) async fn list_jobs(
    State(processing): State<Arc<Processing>>,
) -> ApiResult<Json<Vec<Job>>> {
    let jobs = processing.jobs.lock();
    let jobs: Vec<Job> = jobs.iter().map(|(_, job)| job.clone()).collect();
    Ok(Json(jobs))
}

pub(crate) async fn get_job(
    State(processing): State<Arc<Processing>>,
    Path(job_id): Path<String>,
) -> ApiResult<Json<Job>> {
    let id = JobId(uuid::Uuid::parse_str(&job_id)?);
    let jobs = processing.jobs.lock();
    let job = jobs
        .get(&id)
        .ok_or(StatusError::NotFound(format!("job {id} not found")))?;
    Ok(Json(job.clone()))
}

pub(crate) async fn stop_job(
    State(processing): State<Arc<Processing>>,
    Path(job_id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = JobId(uuid::Uuid::parse_str(&job_id)?);
    let stops = processing.stops.lock();
    let stop = stops
        .get(&id)
        .ok_or(StatusError::NotFound(format!("job {id} is not running")))?;
    stop.stop();
    Ok(StatusCode::OK)
}

pub(crate) async fn get_job_events(
    State(processing): State<Arc<Processing>>,
    Path(job_id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    let rx_result = 'rx: {
        let Ok(uuid) = uuid::Uuid::parse_str(&job_id) else {
            break 'rx Err("Failed to parse job ID");
        };

        let id = JobId(uuid);
        let channels = processing.jobs_channels.lock();
        match channels.get(&id) {
            Some(sender) => Ok(sender.subscribe()),
            None => Err("Not found"),
        }
    };

    let stream = async_stream::stream! {
        match rx_result {
            Ok(mut rx) => {
                while let Ok(job) = rx.recv().await {
                    if let Ok(json) = serde_json::to_string(&job) {
                        yield Ok(Event::default().event("job_update").data(json));
                    }
                }
            }
            Err(error) => {
                yield Ok(Event::default().comment(error));
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
