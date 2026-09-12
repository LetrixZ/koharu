use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use axum::{
    Json,
    body::Bytes,
    extract::{self, Multipart, Path, State},
    http::StatusCode,
};
use koharu_scene::{
    AssetInput, AssetMetadata, AssetRole, At, Commit, PageDraft, Session, Snapshot,
};
use koharu_utils::{
    import::{self, EncodedPage, Format, Page},
    project::{self, PageSummary, ProjectSummary},
};
use rayon::prelude::*;

use super::ApiResult;

#[derive(Clone)]
pub(crate) struct ProjectLibrary {
    root: PathBuf,
}

impl ProjectLibrary {
    pub(crate) fn new() -> Result<Self> {
        let root = project::create_project_library_root()?;
        Ok(Self { root })
    }

    pub(crate) fn list(&self) -> Result<Vec<ProjectSummary>> {
        project::list_projects(&self.root)
    }

    pub(crate) async fn open(&self, name: &str) -> Result<Project> {
        let (name, session) = project::open_project(name, &self.root).await?;
        Ok(Project { name, session })
    }

    pub(crate) async fn create(&self, name: &str) -> Result<String> {
        let (name, _) = project::create_project(name, &self.root).await?;
        Ok(name)
    }

    pub(crate) fn delete(&self, name: &str) -> Result<()> {
        project::delete_project(name, &self.root)
    }
}

pub(crate) struct Project {
    pub(crate) name: String,
    pub(crate) session: Session,
}

impl Project {
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.session.snapshot()
    }

    pub(crate) async fn commit_rebased(
        &mut self,
        patch: koharu_scene::Patch,
    ) -> Result<Option<Commit>> {
        project::commit_rebased(&mut self.session, patch).await
    }

    pub(crate) fn pages(snapshot: &Snapshot) -> Result<Vec<PageSummary>> {
        project::snapshot_pages(snapshot)
    }
}

pub(crate) async fn list_projects(
    State(library): State<ProjectLibrary>,
) -> ApiResult<Json<Vec<ProjectSummary>>> {
    Ok(Json(library.list()?))
}

pub(crate) async fn create_project(
    State(library): State<ProjectLibrary>,
    extract::Json(payload): extract::Json<ProjectSummary>,
) -> ApiResult<Json<ProjectSummary>> {
    let name = library.create(&payload.name).await?;
    Ok(Json(ProjectSummary { name }))
}

pub(crate) async fn delete(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    tokio::task::spawn_blocking(move || library.delete(&name))
        .await
        .context("project deletion worker stopped unexpectedly")??;
    Ok(StatusCode::OK)
}

pub(crate) async fn list_pages(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<PageSummary>>> {
    let project = library.open(&name).await?;
    Ok(Json(Project::pages(&project.snapshot())?))
}

pub(crate) async fn import_pages(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
    mut multipart: Multipart,
) -> ApiResult<StatusCode> {
    let mut project = library.open(&name).await?;

    let mut images: Vec<(PathBuf, Bytes)> = vec![];

    while let Some(field) = multipart.next_field().await.unwrap() {
        if let Some(name) = field.name() {
            if name.to_string() != "images" {
                continue;
            }

            let filename = field.file_name().unwrap().to_string();
            let path = PathBuf::from(filename);

            if !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.parse::<import::Format>().is_ok())
            {
                continue;
            }

            let data = field.bytes().await?;
            images.push((path, data));
        }
    }

    if images.is_empty() {
        return Err(anyhow::anyhow!("no supported images were found in the selection").into());
    }

    let pages = tokio::task::spawn_blocking(move || import_images(images))
        .await
        .context("page import worker stopped unexpectedly")??;
    let page_count = pages.len();

    let source = AssetRole::new("source")?;
    let patch = project.snapshot().patch(|edit| {
        for imported in pages {
            let page = edit.add_page(
                PageDraft::new(
                    imported.name,
                    f64::from(imported.width),
                    f64::from(imported.height),
                ),
                At::End,
            )?;
            edit.set_asset(
                page,
                &source,
                AssetInput::new(
                    imported.bytes,
                    imported.format.to_mime_type(),
                    AssetMetadata {
                        width: Some(imported.width),
                        height: Some(imported.height),
                        attributes: Default::default(),
                    },
                ),
            )?;
        }
        Ok(())
    })?;
    project.session.commit(patch).await?;

    tracing::info!(target: "koharu_metrics", metric = "page_imported", page_count);
    Ok(StatusCode::OK)
}

fn import_images(mut images: Vec<(PathBuf, Bytes)>) -> Result<Vec<Page>> {
    alphanumeric_sort::sort_slice_by_os_str_key(&mut images, |image| &image.0);
    let mut groups = images
        .into_par_iter()
        .map(|(path, bytes)| -> Result<Vec<Page>> {
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| extension.parse::<Format>().ok());
            let encoded = match extension {
                Some(Format::Raster) => vec![EncodedPage {
                    name: path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "page".to_owned()),
                    bytes: bytes.to_vec(),
                }],
                Some(Format::Zip) => import::zip::extract(&path)?,
                Some(Format::Rar) => import::rar::extract(&path)?,
                Some(Format::Pdf) => import::pdf::render(&path)?,
                None => bail!("unsupported page import path {}", path.display()),
            };
            encoded
                .into_iter()
                .map(|source| import::decode(&path, source))
                .collect()
        })
        .collect::<Result<Vec<_>>>()?;
    let page_count = groups.iter().map(Vec::len).sum();
    let mut pages = Vec::with_capacity(page_count);
    for group in &mut groups {
        pages.append(group);
    }
    Ok(pages)
}
