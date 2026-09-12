use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use koharu_scene::{
    AssetRole, EntityId, Geometry as SceneGeometry, Group as SceneGroup,
    RasterLayer as SceneRasterLayer, Session, Snapshot, TextLayout as SceneTextLayout,
};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct PageSummary {
    pub id: EntityId,
    pub label: String,
    pub size: PageSize,
    pub source_asset: Option<String>,
    #[specta(type = f64)]
    pub layer_count: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Type)]
pub struct PageSize {
    pub width: f64,
    pub height: f64,
}

pub fn create_project_library_root() -> Result<PathBuf> {
    let root = dirs::document_dir()
        .context("the Documents directory is unavailable")?
        .join("Koharu");
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create {}", root.display()))?;
    Ok(root)
}

pub fn list_projects(root: &PathBuf) -> Result<Vec<ProjectSummary>> {
    let mut projects = std::fs::read_dir(root)
        .with_context(|| format!("failed to read {}", root.display()))?
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

pub async fn open_project(name: &str, root: &PathBuf) -> Result<(String, Session)> {
    let (name, path) = resolve_project(name, root)?;
    let session = Session::open(&path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    Ok((name, session))
}

pub async fn create_project(name: &str, root: &PathBuf) -> Result<(String, Session)> {
    let (name, path) = resolve_project(name, root)?;
    let session = Session::create(&path)
        .await
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok((name, session))
}

pub fn delete_project(name: &str, root: &PathBuf) -> Result<()> {
    let (_, path) = resolve_project(name, root)?;
    if !path.is_dir() {
        bail!("project {name:?} does not exist");
    }
    std::fs::remove_dir_all(&path).with_context(|| format!("failed to delete {}", path.display()))
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

pub async fn commit_rebased(
    session: &mut Session,
    patch: koharu_scene::Patch,
) -> Result<Option<koharu_scene::Commit>> {
    let snapshot = session.snapshot();
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
    Ok(Some(session.commit(patch).await?))
}

pub fn snapshot_pages(snapshot: &Snapshot) -> Result<Vec<PageSummary>> {
    snapshot
        .pages()
        .map(|page| {
            let value = page.page()?;
            let source_asset = asset_id(snapshot, page.id(), "source")?;
            let layer_count = snapshot
                .descendants(page.id())?
                .map(|entity| is_content_layer(snapshot, entity.id()))
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

fn asset_id(snapshot: &Snapshot, entity: EntityId, role: &str) -> Result<Option<String>> {
    Ok(snapshot
        .asset(entity, &AssetRole::new(role)?)?
        .map(|asset| asset.blob.to_string()))
}

fn is_content_layer(snapshot: &Snapshot, entity: EntityId) -> Result<bool> {
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
