use axum::extract::{self, Path, State};
use koharu_scene::EntityId;
use serde::Deserialize;

use crate::project::ProjectLibrary;

use super::ApiResult;

#[derive(Debug, Deserialize)]
pub(crate) struct RenamePagePayload {
    label: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MovePagePayload {
    index: usize,
}

pub(crate) async fn rename_page(
    State(library): State<ProjectLibrary>,
    Path((name, page)): Path<(String, EntityId)>,
    extract::Json(payload): extract::Json<RenamePagePayload>,
) -> ApiResult<()> {
    let mut project = library.open(&name).await?;
    project.rename_page(page, payload.label).await?;
    Ok(())
}

pub(crate) async fn move_page(
    State(library): State<ProjectLibrary>,
    Path((name, page)): Path<(String, EntityId)>,
    extract::Json(payload): extract::Json<MovePagePayload>,
) -> ApiResult<()> {
    let mut project = library.open(&name).await?;
    project.move_page(page, payload.index).await?;
    Ok(())
}

pub(crate) async fn delete_pages(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
    extract::Json(pages): extract::Json<Vec<EntityId>>,
) -> ApiResult<()> {
    let mut project = library.open(&name).await?;
    project.delete_pages(pages).await?;
    Ok(())
}
