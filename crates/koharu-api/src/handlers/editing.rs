use axum::{
    extract::{self, Path, State},
    http::StatusCode,
};
use koharu_scene::EntityId;

use crate::handlers::{ApiResult, projects::ProjectLibrary};

pub(crate) async fn delete_pages(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
    extract::Json(pages): extract::Json<Vec<EntityId>>,
) -> ApiResult<StatusCode> {
    let mut project = library.open(&name).await?;
    project.delete_pages(pages).await?;
    Ok(StatusCode::OK)
}
