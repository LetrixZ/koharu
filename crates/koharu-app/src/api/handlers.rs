//! REST endpoint handlers for local desktop automation.
//!
//! Each handler maps onto an existing Tauri command's behavior by sharing the
//! same managed state, so an external client drives the exact code paths the
//! desktop interface uses.

use anyhow::{Context as _, Result, anyhow};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use koharu_desktop::Desktop;
use koharu_pipeline::{Operation, PipelineConfig, Scope, Stage};
use koharu_scene::{EntityId, Snapshot, TextLayout as SceneTextLayout};
use koharu_translator::{Language, Model, ModelSelection, Translator};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Cef, Manager as _};

use crate::commands::{
    import,
    lifecycle::{Initialization, add_pages, close_current_project, replace_project},
    output,
    preferences::LanguageChoice,
    processing::{self, Job, JobId, Processing},
    project::Project,
    project::{CurrentProject, PageSize, ProjectInfo, ProjectLibrary, ProjectSummary},
};

use super::ApiState;

/// Error responses follow `{ "error": "<message>" }` with an appropriate
/// status code. Headers are never returned for transport-level failures.
pub(super) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid bearer token".to_owned(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub(crate) fn internal(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{error:#}"),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

#[derive(Serialize)]
pub(super) struct StatusView {
    initialized: bool,
    project: Option<ProjectView>,
    processing: bool,
}

#[derive(Serialize)]
pub(super) struct ProjectView {
    #[serde(flatten)]
    info: ProjectInfo,
    pages: Vec<ApiPage>,
}

/// Page projection for the API: the desktop summary plus translation state.
#[derive(Serialize)]
pub(super) struct ApiPage {
    id: EntityId,
    label: String,
    size: PageSize,
    source_asset: Option<String>,
    layer_count: usize,
    text_layers: usize,
    translated_layers: usize,
    /// `true` when every text layer carries a translation and at least one
    /// text layer exists; pages that were never processed report `false`.
    translated: bool,
}

async fn current_project_view(app: &AppHandle<Cef>) -> Result<Option<ProjectView>> {
    let (info, pages) = {
        let current = app.state::<CurrentProject>();
        let project = current.project.lock().await;
        let Some(project) = project.as_ref() else {
            return Ok(None);
        };
        let snapshot = project.snapshot();
        (project.info(), api_pages(&snapshot)?)
    };
    Ok(Some(ProjectView { info, pages }))
}

fn api_pages(snapshot: &Snapshot) -> Result<Vec<ApiPage>> {
    Project::pages(snapshot)?
        .into_iter()
        .map(|summary| {
            let (text_layers, translated_layers) = page_translation_counts(snapshot, summary.id)?;
            Ok(ApiPage {
                id: summary.id,
                label: summary.label,
                size: summary.size,
                source_asset: summary.source_asset,
                layer_count: summary.layer_count,
                text_layers,
                translated_layers,
                translated: text_layers > 0 && translated_layers == text_layers,
            })
        })
        .collect()
}

fn page_translation_counts(snapshot: &Snapshot, page: EntityId) -> Result<(usize, usize)> {
    let mut text_layers = 0;
    let mut translated = 0;
    for entity in snapshot.descendants(page)? {
        if snapshot
            .component::<SceneTextLayout>(entity.id())?
            .is_none()
        {
            continue;
        }
        text_layers += 1;
        let content = snapshot.text_layer(entity.id())?.content()?;
        if content
            .translation()?
            .is_some_and(|translation| !translation.text.value.trim().is_empty())
        {
            translated += 1;
        }
    }
    Ok((text_layers, translated))
}

async fn open_project_internal(app: &AppHandle<Cef>, name: &str) -> Result<(), ApiError> {
    let library = app.state::<ProjectLibrary>().inner().clone();
    if !library
        .list()
        .map_err(ApiError::internal)?
        .iter()
        .any(|summary| summary.name == name)
    {
        return Err(ApiError::not_found(format!(
            "project {name:?} does not exist"
        )));
    }
    let already_open = {
        let state = app.state::<CurrentProject>();
        let current = state.project.lock().await;
        current.as_ref().is_some_and(|project| project.name == name)
    };
    if already_open {
        // The project's session already holds the exclusive storage lock;
        // reopening it would fail with a lock conflict, so just select it.
        return Ok(());
    }
    let opened = library.open(name).await.map_err(ApiError::internal)?;
    replace_project(app, opened)
        .await
        .map_err(ApiError::internal)
}

pub(super) async fn list_projects(
    State(state): State<ApiState>,
) -> Result<Json<Vec<ProjectSummary>>, ApiError> {
    let library = state.app.state::<ProjectLibrary>().inner().clone();
    let projects = tokio::task::spawn_blocking(move || library.list())
        .await
        .context("project listing worker stopped unexpectedly")??;
    Ok(Json(projects))
}

#[derive(Deserialize)]
pub(super) struct CreateProjectRequest {
    #[serde(default)]
    name: String,
}

pub(super) async fn create_project(
    State(state): State<ApiState>,
    Json(request): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<ProjectView>), ApiError> {
    let name = crate::commands::project::validate_project_name(&request.name)?;
    let library = state.app.state::<ProjectLibrary>().inner().clone();
    if library.list()?.iter().any(|project| project.name == name) {
        return Err(ApiError::conflict(format!(
            "project {name:?} already exists"
        )));
    }
    let opened = library.create(&name).await?;
    replace_project(&state.app, opened).await?;
    let project = current_project_view(&state.app)
        .await?
        .context("the created project could not be opened")?;
    Ok((StatusCode::CREATED, Json(project)))
}

pub(super) async fn get_project(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<Json<ProjectView>, ApiError> {
    // Opening selects the project for subsequent pipeline and export calls.
    open_project_internal(&state.app, &name).await?;
    let project = current_project_view(&state.app)
        .await?
        .context("no project is open")?;
    Ok(Json(project))
}

pub(super) async fn delete_project(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let library = state.app.state::<ProjectLibrary>().inner().clone();
    let projects = library.list()?;
    if !projects.iter().any(|project| project.name == name) {
        return Err(ApiError::not_found(format!(
            "project {name:?} does not exist"
        )));
    }
    let active = state
        .app
        .state::<CurrentProject>()
        .project
        .lock()
        .await
        .as_ref()
        .is_some_and(|project| project.name == name);
    if active {
        close_current_project(&state.app).await?;
    }
    tokio::task::spawn_blocking(move || library.delete(&name))
        .await
        .context("project deletion worker stopped unexpectedly")??;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn import_images(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<ProjectView>, ApiError> {
    open_project_internal(&state.app, &name).await?;
    let mut pages = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("invalid multipart upload: {error}")))?
    {
        let name = match field
            .file_name()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(file_name) => file_name.to_owned(),
            None => format!("page-{}.png", pages.len() + 1),
        };
        let bytes = field
            .bytes()
            .await
            .map_err(|error| {
                ApiError::bad_request(format!("failed to read uploaded image {name}: {error}"))
            })?
            .to_vec();
        let page = import::decode_page(name, bytes)
            .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
        pages.push(page);
    }
    if pages.is_empty() {
        return Err(ApiError::bad_request(
            "image import requires at least one uploaded file",
        ));
    }
    let desktop = state.app.state::<Desktop>();
    let canvas_channel = state.app.state::<crate::commands::canvas::CanvasChannel>();
    let current = state.app.state::<CurrentProject>();
    let mut current = current.project.lock().await;
    let project = current.as_mut().context("no project is open")?;
    add_pages(project, &desktop, &canvas_channel, pages)
        .await
        .map_err(ApiError::internal)?;
    drop(current);
    let project = current_project_view(&state.app)
        .await?
        .context("no project is open")?;
    Ok(Json(project))
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApiPipelineOperation {
    #[default]
    Full,
    Detection,
    Ocr,
    Translation,
    Inpainting,
}

impl From<ApiPipelineOperation> for Operation {
    fn from(value: ApiPipelineOperation) -> Self {
        match value {
            ApiPipelineOperation::Full => Self::Full,
            ApiPipelineOperation::Detection => Self::Only {
                stage: Stage::Detection,
            },
            ApiPipelineOperation::Ocr => Self::Only { stage: Stage::Ocr },
            ApiPipelineOperation::Translation => Self::Only {
                stage: Stage::Translation,
            },
            ApiPipelineOperation::Inpainting => Self::Only {
                stage: Stage::Inpainting,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) struct RunPipelineRequest {
    #[serde(default)]
    operation: ApiPipelineOperation,
    #[serde(default)]
    pages: Vec<String>,
    #[serde(default)]
    elements: Vec<String>,
}

impl RunPipelineRequest {
    fn scope(&self) -> Result<Scope> {
        match (self.pages.is_empty(), self.elements.is_empty()) {
            (true, true) => Ok(Scope::Project),
            (false, true) => Ok(Scope::Pages(parse_entities(&self.pages)?)),
            (true, false) => Ok(Scope::Entities(parse_entities(&self.elements)?)),
            (false, false) => {
                anyhow::bail!("pipeline scope cannot contain both pages and elements")
            }
        }
    }
}

#[derive(Serialize)]
pub(super) struct RunStarted {
    job: String,
    url: String,
}

pub(super) async fn run_pipeline(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Json(request): Json<RunPipelineRequest>,
) -> Result<Json<RunStarted>, ApiError> {
    open_project_internal(&state.app, &name).await?;
    if !state.app.state::<Processing>().stops.lock().is_empty() {
        return Err(ApiError::conflict(
            "another pipeline job is already running",
        ));
    }
    let scope = request.scope()?;
    let job = processing::start_process(&state.app, scope, request.operation.into()).await?;
    Ok(Json(RunStarted {
        job: job.to_string(),
        url: format!("/v1/jobs/{job}"),
    }))
}

pub(super) async fn get_job(
    State(state): State<ApiState>,
    Path(job): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let job = job
        .parse::<JobId>()
        .map_err(|_| ApiError::bad_request("job ID must be a UUID"))?;
    state
        .app
        .state::<Processing>()
        .jobs
        .lock()
        .iter()
        .find(|candidate| candidate.id == job)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::not_found("job not found"))
}

pub(super) async fn stop_job(
    State(state): State<ApiState>,
    Path(job): Path<String>,
) -> Result<StatusCode, ApiError> {
    let job = job
        .parse::<JobId>()
        .map_err(|_| ApiError::bad_request("job ID must be a UUID"))?;
    let stop = state
        .app
        .state::<Processing>()
        .stops
        .lock()
        .get(&job)
        .cloned()
        .ok_or_else(|| ApiError::not_found("job is not running"))?;
    stop.stop();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(super) struct ExportQuery {
    #[serde(default)]
    pages: Option<String>,
}

pub(super) async fn export_zip(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Query(query): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    open_project_internal(&state.app, &name).await?;
    let snapshot = {
        let current = state.app.state::<CurrentProject>();
        let project = current.project.lock().await;
        project.as_ref().context("no project is open")?.snapshot()
    };
    let pages = match query.pages.as_deref() {
        None | Some("") => snapshot.pages().map(|page| page.id()).collect::<Vec<_>>(),
        Some(values) => values
            .split(',')
            .map(parse_entity)
            .collect::<Result<Vec<_>>>()?,
    };
    if pages.is_empty() {
        return Err(ApiError::bad_request("project has no pages to export"));
    }
    let desktop = state.app.state::<Desktop>();
    let bytes = output::export_pages_zip(&desktop, &snapshot, pages).await?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}.zip\""),
        )
        .body(Body::from(bytes))
        .map_err(|error| ApiError::internal(anyhow!(error)))
}

pub(super) async fn status(State(state): State<ApiState>) -> Result<Json<StatusView>, ApiError> {
    let initialized = state.app.state::<Initialization>().is_ready();
    let project = current_project_view(&state.app).await?;
    let processing = !state.app.state::<Processing>().stops.lock().is_empty();
    Ok(Json(StatusView {
        initialized,
        project,
        processing,
    }))
}

fn parse_entities(values: &[String]) -> Result<Vec<EntityId>> {
    values.iter().map(|value| parse_entity(value)).collect()
}

fn parse_entity(value: &str) -> Result<EntityId> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .with_context(|| format!("invalid entity ID {value}"))
}

/// The translation model choices, matching the desktop's model picker.
pub(super) async fn translation_models(
    State(_state): State<ApiState>,
) -> Result<Json<Vec<Model>>, ApiError> {
    Ok(Json(Translator::models().await?))
}

/// Every supported target language as `{ tag, name }` entries.
pub(super) async fn translation_languages(
    State(_state): State<ApiState>,
) -> Result<Json<Vec<LanguageChoice>>, ApiError> {
    let languages = Language::ALL
        .iter()
        .map(|language| LanguageChoice {
            tag: language.tag().to_owned(),
            name: language.to_string(),
        })
        .collect();
    Ok(Json(languages))
}

#[derive(Serialize)]
pub(super) struct TranslationPreferences {
    model: ModelSelection,
    target_language: Language,
}

/// The currently configured translation model and target language.
pub(super) async fn get_translation_preferences(
    State(_state): State<ApiState>,
) -> Result<Json<TranslationPreferences>, ApiError> {
    let translation = PipelineConfig::load()?.read()?.translation.clone();
    Ok(Json(TranslationPreferences {
        model: translation.model,
        target_language: translation.target_language,
    }))
}

#[derive(Deserialize)]
pub(super) struct UpdateTranslationRequest {
    #[serde(default)]
    model: Option<ModelSelection>,
    #[serde(default)]
    target_language: Option<String>,
}

/// Updates the translation preference; omitted fields keep their current
/// value. Returns the resulting configuration.
pub(super) async fn set_translation_preferences(
    State(_state): State<ApiState>,
    Json(request): Json<UpdateTranslationRequest>,
) -> Result<Json<TranslationPreferences>, ApiError> {
    let target_language = request
        .target_language
        .as_deref()
        .map(|tag| {
            tag.parse::<Language>()
                .map_err(|_| ApiError::bad_request(format!("unknown language tag {tag:?}")))
        })
        .transpose()?;
    let config = PipelineConfig::load()?;
    let (model, target_language) = {
        let mut config = config.write()?;
        if let Some(model) = request.model {
            config.translation.model = model;
        }
        if let Some(language) = target_language {
            config.translation.target_language = language;
        }
        let model = config.translation.model.clone();
        let target_language = config.translation.target_language;
        config.save()?;
        (model, target_language)
    };
    Ok(Json(TranslationPreferences {
        model,
        target_language,
    }))
}
