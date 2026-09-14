use std::path::PathBuf;

use anyhow::Context as _;
use axum::{
    Json,
    extract::{self, Multipart, Path, State},
};
use koharu_scene::EntityId;
use serde::Deserialize;

use crate::project::{PageSummary, ProjectLibrary, ProjectState, ProjectSummary};
use crate::{import::Format, project::Page};

use super::{ApiResult, StatusError};

pub(crate) async fn list_projects(
    State(library): State<ProjectLibrary>,
) -> ApiResult<Json<Vec<ProjectSummary>>> {
    Ok(Json(library.list()?))
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CreateProjectPayload {
    name: String,
}

pub(crate) async fn create_project(
    State(library): State<ProjectLibrary>,
    extract::Json(payload): extract::Json<CreateProjectPayload>,
) -> ApiResult<Json<ProjectSummary>> {
    let project = library.create(&payload.name).await?;
    Ok(Json(ProjectSummary { name: project.name }))
}

pub(crate) async fn get_project(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
) -> ApiResult<Json<ProjectState>> {
    let project = library.open(&name).await?;
    Ok(Json(project.state().await?))
}

pub(crate) async fn delete(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
) -> ApiResult<()> {
    tokio::task::spawn_blocking(move || library.delete(&name))
        .await
        .context("project deletion worker stopped unexpectedly")??;
    Ok(())
}

pub(crate) async fn list_pages(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<PageSummary>>> {
    let project = library.open(&name).await?;
    Ok(Json(project.pages()?))
}

pub(crate) async fn get_page(
    State(library): State<ProjectLibrary>,
    Path((name, page)): Path<(String, EntityId)>,
) -> ApiResult<Json<Page>> {
    let project = library.open(&name).await?;
    Ok(Json(project.page(page).await?))
}

pub(crate) async fn import_pages(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
    mut multipart: Multipart,
) -> ApiResult<()> {
    let mut project = library.open(&name).await?;

    let mut images: Vec<(PathBuf, Vec<u8>)> = vec![];

    while let Some(field) = multipart.next_field().await? {
        if let Some(name) = field.name() {
            if name.to_string() != "images" {
                continue;
            }

            let Some(filename) = field.file_name() else {
                continue;
            };

            let path = PathBuf::from(filename);

            if !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.parse::<Format>().is_ok())
            {
                continue;
            }

            let bytes = field.bytes().await?;
            images.push((path, bytes.to_vec()));
        }
    }

    if images.is_empty() {
        return Err(StatusError::BadRequest(
            "no supported images were found in the body".to_string(),
        )
        .into());
    }

    project.import_pages(images).await?;

    Ok(())
}
