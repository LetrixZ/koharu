use std::{collections::HashSet, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use axum::{
    Json,
    extract::{self, Multipart, Path, State},
    http::StatusCode,
};
use koharu_scene::{
    AssetInput, AssetMetadata, AssetRole, At, Commit, EntityId, Geometry as SceneGeometry,
    Group as SceneGroup, PageDraft, RasterLayer as SceneRasterLayer, RemovePolicy, Session,
    Snapshot, TextLayout as SceneTextLayout,
};
use serde::{Deserialize, Serialize};

use crate::handlers::StatusError;

use super::{
    ApiResult,
    import::{Format, import},
};

#[derive(Clone)]
pub(crate) struct ProjectLibrary {
    root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PageSummary {
    pub id: EntityId,
    pub label: String,
    pub size: PageSize,
    pub source_asset: Option<String>,
    pub layer_count: usize,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PageSize {
    pub width: f64,
    pub height: f64,
}

impl ProjectLibrary {
    pub(crate) fn new() -> Result<Self> {
        let root = dirs::document_dir()
            .context("the Documents directory is unavailable")?
            .join("Koharu");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        Ok(Self { root })
    }

    pub(crate) fn list(&self) -> Result<Vec<ProjectSummary>> {
        let mut projects = std::fs::read_dir(self.root.clone())
            .with_context(|| format!("failed to read {}", self.root.display()))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| {
                let path = entry.path();
                let is_project_directory = path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("khrproj"))
                    && (path.join("state-a.khr").is_file() || path.join("state-b.khr").is_file());
                if !is_project_directory {
                    return None;
                }
                let last_used = ["state-a.khr", "state-b.khr"]
                    .into_iter()
                    .filter_map(|file| std::fs::metadata(path.join(file)).ok()?.modified().ok())
                    .max()
                    .unwrap_or(std::time::UNIX_EPOCH);
                Some((
                    last_used,
                    ProjectSummary {
                        name: path.file_stem()?.to_str()?.to_owned(),
                    },
                ))
            })
            .collect::<Vec<_>>();
        projects.sort_unstable_by(|(left_used, left), (right_used, right)| {
            right_used
                .cmp(left_used)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(projects.into_iter().map(|(_, project)| project).collect())
    }

    pub(crate) async fn open(&self, name: &str) -> Result<Project> {
        let (name, path) = resolve_project(name, &self.root)?;
        let session = Session::open(&path).await?;
        Ok(Project { name, session })
    }

    pub(crate) async fn create(&self, name: &str) -> Result<Project> {
        let (name, path) = resolve_project(name, &self.root)?;
        let session = Session::create(&path).await?;
        Ok(Project { name, session })
    }

    pub(crate) fn delete(&self, name: &str) -> Result<()> {
        let (_, path) = resolve_project(name, &self.root)?;
        if !path.is_dir() {
            bail!("project {name:?} does not exist");
        }
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("failed to delete {}", path.display()))
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
        let snapshot = self.session.snapshot();
        let patch = match patch.rebase_on(&snapshot) {
            Ok(patch) => patch,
            Err(
                error @ (koharu_scene::Error::PatchConflict(_)
                | koharu_scene::Error::EntityNotFound(_)
                | koharu_scene::Error::RelationNotFound(_)),
            ) => {
                tracing::debug!(%error, "pipeline output was superseded by a document edit");
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if patch.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.session.commit(patch).await?))
    }

    pub(crate) fn pages(self) -> Result<Vec<PageSummary>> {
        let snapshot = self.snapshot();
        snapshot
            .pages()
            .map(|page| {
                let value = page.page()?;
                let source_asset = asset_id(&snapshot, page.id(), "source")?;
                let layer_count = snapshot
                    .descendants(page.id())?
                    .map(|entity| is_content_layer(&snapshot, entity.id()))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|present| *present)
                    .count()
                    + usize::from(source_asset.is_some());
                Ok(PageSummary {
                    id: page.id(),
                    label: value.label,
                    size: PageSize {
                        width: value.width,
                        height: value.height,
                    },
                    source_asset,
                    layer_count,
                })
            })
            .collect()
    }

    pub(crate) async fn delete_pages(&mut self, pages: Vec<EntityId>) -> Result<()> {
        let snapshot = self.session.snapshot();
        let pages = unique_roots(&snapshot, pages)?;
        let patch = snapshot.patch(|edit| {
            for page in pages {
                edit.remove_entity(page, RemovePolicy::Cascade)?;
            }
            Ok(())
        })?;
        self.session.commit(patch).await?;
        Ok(())
    }
}

pub fn resolve_project(name: &str, root: &PathBuf) -> Result<(String, PathBuf)> {
    let name = validate_project_name(name)?;
    Ok((name.clone(), root.join(format!("{name}.khrproj"))))
}

fn validate_project_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("project name cannot be empty");
    }
    if name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|character| character.is_control() || r#"<>:"/\|?*"#.contains(character))
    {
        bail!("project name contains characters that cannot be used in a file name");
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
    {
        bail!("project name is reserved by Windows");
    }
    Ok(name.to_owned())
}

pub fn asset_id(snapshot: &Snapshot, entity: EntityId, role: &str) -> Result<Option<String>> {
    Ok(snapshot
        .asset(entity, &AssetRole::new(role)?)?
        .map(|asset| asset.blob.to_string()))
}

pub fn is_content_layer(snapshot: &Snapshot, entity: EntityId) -> Result<bool> {
    Ok(snapshot.component::<SceneGroup>(entity)?.is_none() && is_layer(snapshot, entity)?)
}

fn is_layer(snapshot: &Snapshot, entity: EntityId) -> Result<bool> {
    Ok(snapshot.component::<SceneGroup>(entity)?.is_some()
        || snapshot.component::<SceneTextLayout>(entity)?.is_some()
        || snapshot.component::<SceneRasterLayer>(entity)?.is_some()
        || (snapshot.component::<SceneGeometry>(entity)?.is_some()
            && snapshot
                .asset(entity, &AssetRole::new("source")?)?
                .is_some()))
}

pub fn unique_roots(snapshot: &Snapshot, entities: Vec<EntityId>) -> Result<Vec<EntityId>> {
    let selected = entities.into_iter().collect::<HashSet<_>>();
    let mut roots = Vec::new();
    for entity in selected.iter().copied() {
        let mut parent = snapshot.parent(entity)?;
        let mut nested = false;
        while let Some(value) = parent {
            if selected.contains(&value) {
                nested = true;
                break;
            }
            parent = snapshot.parent(value)?;
        }
        if !nested {
            roots.push(entity);
        }
    }
    roots.sort_unstable();
    Ok(roots)
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
    let project = library.create(&payload.name).await?;
    Ok(Json(ProjectSummary { name: project.name }))
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
    Ok(Json(project.pages()?))
}

pub(crate) async fn import_pages(
    State(library): State<ProjectLibrary>,
    Path(name): Path<String>,
    mut multipart: Multipart,
) -> ApiResult<StatusCode> {
    let mut project = library.open(&name).await?;

    let mut images: Vec<(PathBuf, Vec<u8>)> = vec![];

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

    let pages = tokio::task::spawn_blocking(move || import(images))
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
