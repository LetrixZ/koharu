use axum::{
    body::Bytes,
    extract::{self, Path, Query, State},
    http::header,
    response::{IntoResponse as _, Response},
};
use koharu_renderer::Renderer;
use koharu_scene::EntityId;
use serde::Deserialize;

use crate::{
    output::{self, ExportFormat},
    project::ProjectLibrary,
};

use super::{ApiResult, AppState};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExportPageQuery {
    format: Option<ExportFormat>,
}

pub(crate) async fn export_page(
    State(state): State<AppState>,
    State(library): State<ProjectLibrary>,
    State(renderer): State<Renderer>,
    Query(query): Query<ExportPageQuery>,
    Path((name, page)): Path<(String, EntityId)>,
) -> ApiResult<Response> {
    let format = query.format.unwrap_or(ExportFormat::Png);

    let project = library.open(&name).await?;

    let (name, mimetype, bytes) =
        output::export_page(project, page, format, state.rasterizer().await?, renderer).await?;

    let mut response = Bytes::from(bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&mimetype)?,
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))?,
    );
    Ok(response)
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExportPayload {
    #[serde(default)]
    pages: Vec<EntityId>,
    format: ExportFormat,
}

pub(crate) async fn export_pages(
    State(state): State<AppState>,
    State(library): State<ProjectLibrary>,
    State(renderer): State<Renderer>,
    Path(name): Path<String>,
    extract::Json(payload): extract::Json<ExportPayload>,
) -> ApiResult<Response> {
    let project = library.open(&name).await?;

    let (name, mimetype, bytes) = output::export_pages(
        project,
        payload.pages,
        payload.format,
        state.rasterizer().await?,
        renderer,
    )
    .await?;

    let mut response = Bytes::from(bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&mimetype)?,
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))?,
    );
    Ok(response)
}
