//! Embedded REST API for local automation of the desktop application.
//!
//! The server is opt-in, controlled by the Settings → API tab. The persisted
//! [`ApiConfig`] selects whether the server runs, which host it binds
//! (default `127.0.0.1`; `0.0.0.0` exposes it on the local network), and
//! which port it serves on. The bearer token is optional and stored as a
//! Koharu secret: when no key is configured, requests are accepted without
//! authentication.
//!
//! [`ApiServer`] is managed Tauri state that owns the running server. It is
//! started at application setup and restarted whenever the settings change:
//! the old task is aborted and awaited so its socket is fully released
//! before the new listener binds. The chosen host, port, and token (when any)
//! are logged and also written to `~/.koharu/api.json` so external tools can
//! discover the endpoint without parsing logs; the file is removed when the
//! server is disabled.
//!
//! Handlers reuse the same managed state and pipeline machinery as the Tauri
//! commands, so an API call observes and mutates the same project, processing
//! jobs, and rendered canvas as the desktop window.

mod handlers;

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::header::AUTHORIZATION,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use koharu_secrets::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Cef};

use self::handlers::{
    ApiError, create_project, delete_project, export_zip, get_job, get_jobs, get_project,
    get_translation_preferences, import_images, list_projects, run_pipeline,
    set_translation_preferences, status, stop_job, translation_languages, translation_models,
};

/// Key under which the API bearer token is stored in the secrets store.
pub(crate) const API_TOKEN_KEY: &str = "api_token";

/// Default host and port when nothing is configured.
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 4000;

/// Upper bound for multipart image uploads and JSON request bodies.
const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;

/// Persisted REST API settings. Lives in the `[api]` TOML section.
///
/// `host` is the bind address: `127.0.0.1` for loopback only, `0.0.0.0` to
/// accept connections from the local network. A `port` of `0` is not a valid
/// choice; the default port applies when the field is missing.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ApiConfig {
    pub(crate) enabled: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
        }
    }
}

impl ApiConfig {
    pub(crate) fn load() -> Result<koharu_config::Config<Self>> {
        koharu_config::load("api")
    }
}

/// Application state shared by the router and its handlers.
#[derive(Clone)]
pub(crate) struct ApiState {
    app: AppHandle<Cef>,
    token: Option<Arc<str>>,
}

/// Tauri-managed state owning the running REST API server, if any.
pub(crate) struct ApiServer {
    app: AppHandle<Cef>,
    current: Mutex<Option<RunningServer>>,
}

struct RunningServer {
    port: u16,
    task: tauri::async_runtime::JoinHandle<()>,
}

struct Started {
    listener: tokio::net::TcpListener,
    router: Router,
    port: u16,
    token: Option<String>,
}

impl ApiServer {
    pub(crate) fn new(app: AppHandle<Cef>) -> Self {
        Self {
            app,
            current: Mutex::new(None),
        }
    }

    /// Applies the persisted settings: stops any running server, then starts
    /// one when enabled. Returns the listening information when running.
    pub(crate) async fn apply(&self) -> Result<Option<ServerInfo>> {
        let stopped = self
            .current
            .lock()
            .expect("the API server lock is poisoned")
            .take();
        if let Some(running) = stopped {
            running.task.abort();
            // Wait for the listener to actually close. Aborting alone lets the
            // old task race the new bind and fail with "address already in use"
            // when the port is reused unchanged.
            let _ = running.task.await;
        }
        let (enabled, host, port) = {
            let config = ApiConfig::load()?;
            let config = config.read()?;
            let host = if config.host.trim().is_empty() {
                DEFAULT_HOST.to_owned()
            } else {
                config.host.clone()
            };
            // Older configs may hold 0, which used to mean "OS-assigned port".
            let port = if config.port == 0 {
                DEFAULT_PORT
            } else {
                config.port
            };
            (config.enabled, host, port)
        };
        if !enabled {
            remove_discovery();
            tracing::debug!("the Koharu REST API is disabled");
            return Ok(None);
        }
        match self.start(&host, port) {
            Ok(started) => {
                let Started {
                    listener,
                    router,
                    port,
                    token,
                } = started;
                let task = tauri::async_runtime::spawn(async move {
                    if let Err(error) = axum::serve(listener, router).await {
                        tracing::error!(%error, "the REST API server stopped unexpectedly");
                    }
                });
                let mut current = self
                    .current
                    .lock()
                    .expect("the API server lock is poisoned");
                *current = Some(RunningServer { port, task });
                let info = ServerInfo { host, port, token };
                write_discovery(&info)?;
                Ok(Some(info))
            }
            Err(error) => {
                // The previous listener is already gone; keep discovery accurate.
                remove_discovery();
                Err(error)
            }
        }
    }

    /// Binds the listener and builds the router for a fresh server instance.
    fn start(&self, host: &str, port: u16) -> Result<Started> {
        let token = stored_token()?;
        let listener = std::net::TcpListener::bind((host, port))
            .with_context(|| format!("failed to bind the REST API on {host}:{port}"))?;
        listener
            .set_nonblocking(true)
            .context("failed to configure the REST API listener")?;
        let port = listener
            .local_addr()
            .context("failed to read the REST API port")?
            .port();
        let listener = tokio::net::TcpListener::from_std(listener)
            .context("failed to register the REST API listener")?;
        let state = ApiState {
            app: self.app.clone(),
            token: token.clone().map(Arc::from),
        };
        let router = router(state.clone());
        Ok(Started {
            listener,
            router,
            port,
            token,
        })
    }

    /// The port the server is currently listening on, if enabled.
    pub(crate) fn listening(&self) -> Option<u16> {
        self.current
            .lock()
            .expect("the API server lock is poisoned")
            .as_ref()
            .map(|running| running.port)
    }
}

fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route(
            "/v1/projects/{name}",
            get(get_project).delete(delete_project),
        )
        .route("/v1/projects/{name}/images", post(import_images))
        .route("/v1/projects/{name}/pipeline", post(run_pipeline))
        .route("/v1/jobs", get(get_jobs))
        .route("/v1/jobs/{job}", get(get_job))
        .route("/v1/jobs/{job}/stop", post(stop_job))
        .route("/v1/translation/models", get(translation_models))
        .route("/v1/translation/languages", get(translation_languages))
        .route(
            "/v1/translation/preferences",
            get(get_translation_preferences).put(set_translation_preferences),
        )
        .route("/v1/projects/{name}/export.zip", get(export_zip))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn authenticate(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let authorized = match state.token.as_deref() {
        // No key configured: the API is open.
        None => true,
        Some(token) => request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| token_matches(value, token)),
    };
    if !authorized {
        return ApiError::unauthorized().into_response();
    }
    next.run(request).await
}

/// Constant-time-ish comparison of the presented bearer header with the token.
fn token_matches(actual: &str, expected: &str) -> bool {
    let expected = format!("Bearer {expected}");
    let actual = actual.as_bytes();
    let expected = expected.as_bytes();
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .iter()
        .zip(expected)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

pub(crate) struct ServerInfo {
    pub(crate) host: String,
    pub(crate) port: u16,
    /// `None` when no key is configured and the API is open.
    pub(crate) token: Option<String>,
}

/// The stored bearer token, or `None` when the API is left unauthenticated.
fn stored_token() -> Result<Option<String>> {
    Ok(koharu_secrets::get(API_TOKEN_KEY)?
        .filter(|token| !token.expose_secret().trim().is_empty())
        .map(|token| token.expose_secret().to_owned()))
}

fn discovery_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(home.join(".koharu").join("api.json"))
}

fn write_discovery(info: &ServerInfo) -> Result<()> {
    let path = discovery_path()?;
    let directory = path.parent().context("discovery path has no parent")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let value = serde_json::json!({
        "host": info.host,
        "port": info.port,
        "token": info.token,
    });
    let bytes = serde_json::to_vec_pretty(&value)
        .context("failed to serialize the REST API discovery file")?;
    std::fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure {}", path.display()))?;
    }
    Ok(())
}

/// Removes the discovery file so tools do not connect to a disabled server.
fn remove_discovery() {
    if let Ok(path) = discovery_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::token_matches;

    #[test]
    fn bearer_tokens_match_only_when_equal() {
        let token = "0123456789abcdef";
        assert!(token_matches(&format!("Bearer {token}"), token));
        assert!(!token_matches("", token));
        assert!(!token_matches(
            &format!("Bearer {}", "fedcba9876543210"),
            token
        ));
        assert!(!token_matches(&format!("Bearer {token} "), token));
        assert!(!token_matches(
            &format!("Bearer {}", token.to_ascii_uppercase()),
            token
        ));
    }
}
