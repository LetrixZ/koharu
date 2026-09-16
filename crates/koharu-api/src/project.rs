use std::{collections::HashSet, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use koharu_scene::{
    AssetInput, AssetMetadata, AssetRole, At, Commit, EntityId, PageDraft, RasterLayer,
    RasterLayerKind, Region, RemovePolicy, Session, Snapshot, SourceText, TextContent, Translation,
};
use serde::Serialize;

use crate::import::import;

#[derive(Clone)]
pub(crate) struct ProjectLibrary {
    root: PathBuf,
}

pub(crate) struct Project {
    pub(crate) name: String,
    pub(crate) session: Session,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProjectState {
    pub(crate) name: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProjectSummary {
    pub(crate) name: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Page {
    pub(crate) id: EntityId,
    pub(crate) label: String,
    pub(crate) size: PageSize,
    pub(crate) stages: StageProgress,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PageSummary {
    pub(crate) id: EntityId,
    pub(crate) label: String,
    pub(crate) size: PageSize,
    pub(crate) stages: StageProgress,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct PageSize {
    pub(crate) width: f64,
    pub(crate) height: f64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StageProgress {
    pub(crate) detection: bool,
    pub(crate) ocr: bool,
    pub(crate) translation: bool,
    pub(crate) inpainting: bool,
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

    pub(crate) async fn create(&self, name: &str) -> Result<Project> {
        let (name, path) = self.resolve(name)?;
        Project::create(name, path).await
    }

    pub(crate) async fn open(&self, name: &str) -> Result<Project> {
        let (name, path) = self.resolve(name)?;
        Project::open(name, path).await
    }

    pub(crate) fn delete(&self, name: &str) -> Result<()> {
        let (_, path) = self.resolve(name)?;
        if !path.is_dir() {
            bail!("project {name:?} does not exist");
        }
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("failed to delete {}", path.display()))
    }

    fn resolve(&self, name: &str) -> Result<(String, PathBuf)> {
        let name = validate_project_name(name)?;
        Ok((name.clone(), self.root.join(format!("{name}.khrproj"))))
    }
}

impl Project {
    pub(crate) async fn create(name: String, path: PathBuf) -> Result<Self> {
        let session = Session::create(&path)
            .await
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self::new(session, name))
    }

    pub(crate) async fn open(name: String, path: PathBuf) -> Result<Self> {
        let session = Session::open(&path)
            .await
            .with_context(|| format!("failed to open {}", path.display()))?;
        Ok(Self::new(session, name))
    }

    fn new(session: Session, name: String) -> Self {
        Self { session, name }
    }

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
                koharu_scene::Error::PatchConflict(_)
                | koharu_scene::Error::EntityNotFound(_)
                | koharu_scene::Error::RelationNotFound(_),
            ) => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if patch.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.session.commit(patch).await?))
    }

    pub(crate) fn pages(&self) -> Result<Vec<PageSummary>> {
        let snapshot = self.snapshot();
        snapshot
            .pages()
            .map(|page| {
                let value = page.page()?;
                Ok(PageSummary {
                    id: page.id(),
                    label: value.label,
                    size: PageSize {
                        width: value.width,
                        height: value.height,
                    },
                    stages: compute_stage_state(&snapshot, page.id())?,
                })
            })
            .collect()
    }

    pub(crate) async fn import_pages(&mut self, images: Vec<(PathBuf, Vec<u8>)>) -> Result<()> {
        let pages = tokio::task::spawn_blocking(move || import(images))
            .await
            .context("page import worker stopped unexpectedly")??;

        let source = AssetRole::new("source")?;
        let patch = self.snapshot().patch(|edit| {
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
        self.session.commit(patch).await?;
        Ok(())
    }

    pub(crate) async fn rename_page(&mut self, page: EntityId, label: String) -> Result<()> {
        let snapshot = self.snapshot();
        let current = snapshot.page(page)?.page()?;
        let patch = snapshot.patch(|edit| {
            edit.set_page(page, PageDraft::new(label, current.width, current.height))
        })?;
        self.session.commit(patch).await?;
        Ok(())
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

    pub(crate) async fn move_page(&mut self, page: EntityId, index: usize) -> Result<()> {
        let snapshot = self.snapshot();
        let siblings = snapshot.pages().map(|page| page.id()).collect::<Vec<_>>();
        let at = placement(&siblings, page, index);
        let patch = snapshot.patch(|edit| edit.move_entity(page, None, at))?;
        self.session.commit(patch).await?;
        Ok(())
    }

    pub(crate) async fn page(&self, page: EntityId) -> Result<Page> {
        let snapshot = self.snapshot();
        let value = snapshot.page(page)?.page()?;
        Ok(Page {
            id: page,
            label: value.label,
            size: PageSize {
                width: value.width,
                height: value.height,
            },
            stages: compute_stage_state(&snapshot, page)?,
        })
    }

    pub(crate) async fn state(&self) -> Result<ProjectState> {
        Ok(ProjectState {
            name: self.name.clone(),
        })
    }
}

fn unique_roots(snapshot: &Snapshot, entities: Vec<EntityId>) -> Result<Vec<EntityId>> {
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

fn placement(siblings: &[EntityId], moving: EntityId, index: usize) -> At {
    siblings
        .iter()
        .copied()
        .filter(|entity| *entity != moving)
        .nth(index)
        .map_or(At::End, At::Before)
}

fn compute_stage_state(snapshot: &Snapshot, page: EntityId) -> Result<StageProgress> {
    let mut state = StageProgress {
        detection: false,
        ocr: false,
        translation: false,
        inpainting: false,
    };

    for descendant in snapshot.descendants(page)? {
        let id = descendant.id();

        if snapshot.component::<Region>(id)?.is_some() {
            state.detection = true;
        }

        if snapshot
            .component::<RasterLayer>(id)?
            .is_some_and(|layer| layer.kind == RasterLayerKind::Cleanup)
        {
            state.inpainting = true;
        }
    }

    for descendant in snapshot.descendants(page)? {
        let id = descendant.id();
        if snapshot.component::<TextContent>(id)?.is_some() {
            if snapshot.component::<SourceText>(id)?.is_some() {
                state.ocr = true;
            }
            if snapshot.component::<Translation>(id)?.is_some() {
                state.translation = true;
            }
        }
    }

    Ok(state)
}
