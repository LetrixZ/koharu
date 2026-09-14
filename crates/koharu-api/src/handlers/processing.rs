use std::{convert::Infallible, sync::Arc};

use anyhow::Result;
use axum::{
    Json,
    extract::{self, Path, State},
    response::{
        Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::stream::Stream;
use koharu_pipeline::Pipeline;
use serde::{Deserialize, Serialize};

use crate::{
    processing::{self, Job, JobId, JobState, Processing},
    project::ProjectLibrary,
};

use super::{ApiResult, StatusError};

#[derive(Debug, Deserialize)]
pub(crate) struct ProcessPayload {
    #[serde(flatten)]
    scope: koharu_pipeline::Scope,
    #[serde(flatten)]
    operation: koharu_pipeline::Operation,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProcessBody {
    job_id: String,
}

pub(crate) async fn process(
    State(processing): State<Arc<Processing>>,
    State(pipeline): State<Pipeline>,
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
    extract::Json(payload): extract::Json<ProcessPayload>,
) -> ApiResult<Json<ProcessBody>> {
    let project = library.open(&name).await?;

    let id = processing::process(
        project,
        payload.scope,
        payload.operation,
        pipeline,
        processing,
    )
    .await?;

    Ok(Json(ProcessBody {
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
    Path(id): Path<String>,
) -> ApiResult<Json<Job>> {
    let id = parse_job_id(&id)?;
    let jobs = processing.jobs.lock();
    let job = jobs
        .get(&id)
        .ok_or(StatusError::NotFound(format!("job {id} not found")))?;
    Ok(Json(job.clone()))
}

pub(crate) async fn stop_job(
    State(processing): State<Arc<Processing>>,
    Path(id): Path<String>,
) -> ApiResult<()> {
    let id = parse_job_id(&id)?;
    let stops = processing.stops.lock();
    let stop = stops
        .get(&id)
        .ok_or(StatusError::NotFound(format!("job {id} is not running")))?;
    stop.stop();
    Ok(())
}

pub(crate) async fn delete_job(
    State(processing): State<Arc<Processing>>,
    Path(id): Path<String>,
) -> ApiResult<()> {
    let id = parse_job_id(&id)?;
    let mut jobs = processing.jobs.lock();
    let job = jobs
        .get(&id)
        .ok_or(StatusError::NotFound(format!("job {id} not found")))?;
    if let JobState::Running = job.state {
        return Err(StatusError::BadRequest(format!("job {id} is running")).into());
    }
    jobs.remove(&id);
    Ok(())
}

pub(crate) async fn get_job_events(
    State(processing): State<Arc<Processing>>,
    Path(id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    let rx_result = 'rx: {
        let Ok(uuid) = uuid::Uuid::parse_str(&id) else {
            break 'rx Err(format!("failed to parse job id {id}"));
        };

        let id = JobId(uuid);
        let jobs = processing.jobs.lock();
        let Some(job) = jobs.get(&id) else {
            break 'rx Err(format!("job {id} not found"));
        };
        let channels = processing.jobs_channels.lock();
        match channels.get(&id) {
            Some(sender) => Ok((job.clone(), sender.subscribe())),
            None => Err(format!("job {id} not found")),
        }
    };

    let stream = async_stream::stream! {
        match rx_result {
            Ok((initial_job, mut rx)) => {
                if let Ok(json) = serde_json::to_string(&initial_job) {
                    yield Ok(Event::default().event("job_update").data(json));
                }

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

fn parse_job_id(id: &str) -> Result<JobId> {
    Ok(JobId(uuid::Uuid::parse_str(&id).map_err(|_| {
        StatusError::BadRequest(format!("failed to parse job id {id}"))
    })?))
}
