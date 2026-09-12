use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::extract::FromRef;
use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use koharu_pipeline::Pipeline;
use koharu_rasterizer::Rasterizer;
use koharu_renderer::Renderer;
use serde::Serialize;
use tokio::sync::OnceCell;

use super::handlers::{processing::Processing, projects::ProjectLibrary};

mod output;
mod preferences;
mod processing;
mod projects;

#[derive(Clone, FromRef)]
pub(crate) struct AppState {
    pub(crate) library: ProjectLibrary,
    pub(crate) pipeline: Pipeline,
    pub(crate) processing: Arc<Processing>,
    pub(crate) renderer: Renderer,
    pub(crate) rasterizer: OnceCell<Arc<Rasterizer>>,
}

impl AppState {
    pub async fn rasterizer(&self) -> Result<Arc<Rasterizer>> {
        self.rasterizer
            .get_or_try_init(|| async {
                let rasterizer = tokio::task::spawn_blocking(Rasterizer::new)
                    .await
                    .context("native rasterizer initialization worker stopped unexpectedly")??;
                Ok::<_, anyhow::Error>(Arc::new(rasterizer))
            })
            .await
            .cloned()
    }
}

pub(crate) struct Error(anyhow::Error);

pub type ApiResult<T, E = Error> = std::result::Result<T, E>;

#[derive(Serialize)]
struct Message {
    error: String,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(Message {
                error: format!("{}", self.0),
            }),
        )
            .into_response()
    }
}

impl<E> From<E> for Error
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

pub fn router(pipeline: Pipeline) -> Result<Router> {
    let state = AppState {
        library: ProjectLibrary::new()?,
        pipeline,
        processing: Arc::new(Processing::default()),
        renderer: Renderer::new()?,
        rasterizer: OnceCell::new(),
    };

    // TODO: Add more commands from desktop side
    Ok(Router::new()
        .route("/projects", get(projects::list_projects))
        .route("/projects", post(projects::create_project))
        .route("/projects/{name}/pages", get(projects::list_pages))
        .route("/projects/{name}/pages", post(projects::import_pages))
        .route("/projects/{name}/delete", post(projects::delete))
        .route("/projects/{name}/process", post(processing::process))
        .route("/projects/{name}/export", post(output::export_pages))
        .route("/jobs/{id}", get(processing::get_job))
        .route("/jobs/{id}/events", get(processing::get_job_events))
        .route("/jobs/{id}/stop", post(processing::stop_job))
        .route("/preferences", get(preferences::get_preferences))
        .route("/preferences", put(preferences::save_preferences))
        .route(
            "/preferences/models",
            get(preferences::get_translation_models),
        )
        .layer(DefaultBodyLimit::max(104857600)) // 100MB
        .with_state(state))
}
