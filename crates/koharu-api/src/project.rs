use std::{collections::HashSet, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use koharu_scene::{
    AssetInput, AssetMetadata, AssetRole, At, Commit, EntityId, PageDraft, RemovePolicy, Session,
    Snapshot, TextLayout as SceneTextLayout,
};
use serde::{Deserialize, Serialize};

use super::import::import;

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
    pub(crate) translation_state: TranslationState,
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
    pub(crate) text_layers: Vec<TextLayer>,
    pub(crate) translation_state: TranslationState,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TextLayer {
    pub(crate) id: EntityId,
    pub(crate) source: Option<SourceText>,
    pub(crate) translation: Option<Translation>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SourceText {
    pub(crate) text: String,
    pub(crate) language: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Translation {
    pub(crate) text: String,
    pub(crate) language: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranslationState {
    NoText,
    Untranslated,
    Translated,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PageSummary {
    pub(crate) id: EntityId,
    pub(crate) label: String,
    pub(crate) size: PageSize,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct PageSize {
    pub(crate) width: f64,
    pub(crate) height: f64,
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
        let text_layers = collect_text_layers(&snapshot, page)?;
        let translation_state = get_translation_state(&text_layers);
        Ok(Page {
            id: page,
            label: value.label,
            size: PageSize {
                width: value.width,
                height: value.height,
            },
            text_layers,
            translation_state,
        })
    }

    pub(crate) async fn state(&self) -> Result<ProjectState> {
        Ok(ProjectState {
            name: self.name.clone(),
            translation_state: compute_project_translation_state(&self.session)?,
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

fn collect_text_layers(snapshot: &Snapshot, page: EntityId) -> Result<Vec<TextLayer>> {
    let mut text_layers = Vec::new();
    for descendant in snapshot.descendants(page)? {
        let child = descendant.id();
        if snapshot.component::<SceneTextLayout>(child)?.is_none() {
            continue;
        }
        let text_layer = snapshot.text_layer(child)?;
        let content = text_layer.content()?;
        let source = content.source()?.map(|source| SourceText {
            text: source.text.value,
            language: source.language.map(|language| language.to_string()),
        });
        let translation = content.translation()?.map(|translation| Translation {
            text: translation.text.value,
            language: translation.language.map(|language| language.to_string()),
        });
        text_layers.push(TextLayer {
            id: child,
            source,
            translation,
        });
    }
    Ok(text_layers)
}

fn get_translation_state(text_layers: &[TextLayer]) -> TranslationState {
    if text_layers.is_empty() {
        return TranslationState::NoText;
    }

    if text_layers.iter().all(|layer| layer.translation.is_some()) {
        TranslationState::Translated
    } else {
        TranslationState::Untranslated
    }
}

fn compute_project_translation_state(session: &Session) -> Result<TranslationState> {
    let snapshot = session.snapshot();
    let states: Vec<TranslationState> = snapshot
        .pages()
        .map(|page| {
            let text_layers = collect_text_layers(&snapshot, page.id())?;
            Ok(get_translation_state(&text_layers))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(aggregate_translation_states(&states))
}

fn aggregate_translation_states(states: &[TranslationState]) -> TranslationState {
    if states.is_empty()
        || states
            .iter()
            .all(|state| *state == TranslationState::NoText)
    {
        TranslationState::NoText
    } else if states
        .iter()
        .all(|s| matches!(s, TranslationState::Translated | TranslationState::NoText))
    {
        TranslationState::Translated
    } else {
        TranslationState::Untranslated
    }
}
