use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::config::{Config, EventSpec};
use crate::state::SessionState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalEventMessage {
    pub workflow_name: String,
}

#[derive(Debug, Clone)]
pub struct ExternalEventEnvironment {
    pub base_url: String,
    pub token: String,
    pub event_urls: BTreeMap<String, String>,
}

pub struct ExternalEventServer {
    environment: ExternalEventEnvironment,
    task: tokio::task::JoinHandle<()>,
}

impl ExternalEventServer {
    pub async fn start(
        config: &Config,
        state: SessionState,
        sender: mpsc::UnboundedSender<ExternalEventMessage>,
    ) -> Result<Option<Self>> {
        if config.event.is_empty() {
            return Ok(None);
        }

        let bind_addr: SocketAddr =
            config.event_server.bind.parse().with_context(|| {
                format!("invalid event server bind '{}'", config.event_server.bind)
            })?;
        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("failed to bind external event server at {bind_addr}"))?;
        let local_addr = listener
            .local_addr()
            .context("failed to read external event server address")?;
        let token = random_token();
        let environment = ExternalEventEnvironment {
            base_url: format!("http://{local_addr}"),
            token: token.clone(),
            event_urls: config
                .event
                .keys()
                .map(|name| (name.clone(), format!("http://{local_addr}/events/{name}")))
                .collect(),
        };
        let app_state = AppState {
            events: compile_event_specs(&config.event)?,
            token,
            state,
            sender,
        };
        let app = Router::new()
            .route("/events/{name}", post(handle_event))
            .with_state(Arc::new(app_state));
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                error!("external event server stopped unexpectedly: {}", error);
            }
        });

        info!("listening for external events at {}", environment.base_url);
        Ok(Some(Self { environment, task }))
    }

    pub fn environment(&self) -> &ExternalEventEnvironment {
        &self.environment
    }
}

impl Drop for ExternalEventServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct AppState {
    events: BTreeMap<String, CompiledEventSpec>,
    token: String,
    state: SessionState,
    sender: mpsc::UnboundedSender<ExternalEventMessage>,
}

#[derive(Clone)]
struct CompiledEventSpec {
    state_key: String,
    workflow: String,
    pattern: Option<Regex>,
}

#[derive(Debug, Deserialize)]
struct EventPayload {
    value: String,
}

async fn handle_event(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<EventPayload>,
) -> StatusCode {
    let Some(spec) = state.events.get(&name) else {
        return StatusCode::NOT_FOUND;
    };
    if !token_matches(headers, &state.token) {
        return StatusCode::UNAUTHORIZED;
    }
    if let Some(pattern) = &spec.pattern
        && !pattern.is_match(&payload.value)
    {
        return StatusCode::BAD_REQUEST;
    }

    match state
        .state
        .set_if_changed(&spec.state_key, Value::String(payload.value))
    {
        Ok(true) => {
            match state.sender.send(ExternalEventMessage {
                workflow_name: spec.workflow.clone(),
            }) {
                Ok(()) => StatusCode::NO_CONTENT,
                Err(error) => {
                    error!(
                        "failed to dispatch external event '{}' to workflow '{}': {}",
                        name, spec.workflow, error
                    );
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }
        }
        Ok(false) => StatusCode::NO_CONTENT,
        Err(err) => {
            error!(
                "failed to persist external event '{}' to state key '{}': {}",
                name, spec.state_key, err
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

fn token_matches(headers: HeaderMap, token: &str) -> bool {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value == format!("Bearer {token}")
}

fn compile_event_specs(
    events: &BTreeMap<String, EventSpec>,
) -> Result<BTreeMap<String, CompiledEventSpec>> {
    events
        .iter()
        .map(|(name, event)| {
            Ok((
                name.clone(),
                CompiledEventSpec {
                    state_key: event.state_key.clone(),
                    workflow: event.workflow.clone(),
                    pattern: match &event.pattern {
                        Some(pattern) => Some(Regex::new(pattern)?),
                        None => None,
                    },
                },
            ))
        })
        .collect()
}

fn random_token() -> String {
    use rand::{
        SeedableRng,
        distr::{Alphanumeric, SampleString},
        rngs::StdRng,
    };

    Alphanumeric.sample_string(&mut StdRng::from_os_rng(), 32)
}

pub fn apply_external_event_env(
    env: &mut BTreeMap<String, String>,
    runtime: Option<&ExternalEventEnvironment>,
) {
    let Some(runtime) = runtime else {
        return;
    };
    env.insert("DEVLOOP_EVENTS_BASE_URL".into(), runtime.base_url.clone());
    env.insert("DEVLOOP_EVENTS_TOKEN".into(), runtime.token.clone());
    for (name, url) in &runtime.event_urls {
        env.insert(event_url_env_var(name), url.clone());
    }
}

fn event_url_env_var(name: &str) -> String {
    let suffix = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("DEVLOOP_EVENT_{}_URL", suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EventServerConfig, WorkflowSpec, WorkflowStep};
    use std::path::PathBuf;

    fn base_config() -> Config {
        Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        }
    }

    #[test]
    fn apply_external_event_env_exports_base_url_token_and_event_url() {
        let runtime = ExternalEventEnvironment {
            base_url: "http://127.0.0.1:12345".into(),
            token: "secret".into(),
            event_urls: BTreeMap::from([(
                "browser_path".into(),
                "http://127.0.0.1:12345/events/browser_path".into(),
            )]),
        };
        let mut env = BTreeMap::new();

        apply_external_event_env(&mut env, Some(&runtime));

        assert_eq!(
            env.get("DEVLOOP_EVENTS_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:12345")
        );
        assert_eq!(
            env.get("DEVLOOP_EVENTS_TOKEN").map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            env.get("DEVLOOP_EVENT_BROWSER_PATH_URL")
                .map(String::as_str),
            Some("http://127.0.0.1:12345/events/browser_path")
        );
    }

    #[tokio::test]
    async fn event_server_updates_state_and_emits_workflow_message() {
        let mut config = base_config();
        config.workflow.insert(
            "publish_post_url".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "ok".into(),
                    style: crate::config::LogStyle::Plain,
                }],
                triggers: vec![],
            },
        );
        config.event.insert(
            "browser_path".into(),
            EventSpec {
                state_key: "current_browser_path".into(),
                workflow: "publish_post_url".into(),
                pattern: Some("^/posts/[a-z0-9-]+$".into()),
            },
        );
        let state_path = std::env::temp_dir().join(format!(
            "devloop-external-events-{}.json",
            std::process::id()
        ));
        let state = SessionState::load(state_path.clone()).expect("load state");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let server = ExternalEventServer::start(&config, state.clone(), sender)
            .await
            .expect("start external event server")
            .expect("event server");

        let client = reqwest::Client::new();
        let response = client
            .post(
                server
                    .environment()
                    .event_urls
                    .get("browser_path")
                    .expect("browser path url"),
            )
            .bearer_auth(&server.environment().token)
            .json(&serde_json::json!({ "value": "/posts/example-post" }))
            .send()
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            state
                .get_string("current_browser_path")
                .expect("get current_browser_path")
                .as_deref(),
            Some("/posts/example-post")
        );
        assert_eq!(
            receiver.recv().await,
            Some(ExternalEventMessage {
                workflow_name: "publish_post_url".into(),
            })
        );

        let response = client
            .post(
                server
                    .environment()
                    .event_urls
                    .get("browser_path")
                    .expect("browser path url"),
            )
            .bearer_auth(&server.environment().token)
            .json(&serde_json::json!({ "value": "/posts/example-post" }))
            .send()
            .await
            .expect("send duplicate request");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(receiver.try_recv().is_err());

        let _ = std::fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn event_server_rejects_wrong_token() {
        let mut config = base_config();
        config.workflow.insert(
            "publish_post_url".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "ok".into(),
                    style: crate::config::LogStyle::Plain,
                }],
                triggers: vec![],
            },
        );
        config.event.insert(
            "browser_path".into(),
            EventSpec {
                state_key: "current_browser_path".into(),
                workflow: "publish_post_url".into(),
                pattern: None,
            },
        );
        let state_path = std::env::temp_dir().join(format!(
            "devloop-external-events-unauthorized-{}.json",
            std::process::id()
        ));
        let state = SessionState::load(state_path.clone()).expect("load state");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let server = ExternalEventServer::start(&config, state.clone(), sender)
            .await
            .expect("start external event server")
            .expect("event server");

        let client = reqwest::Client::new();
        let response = client
            .post(
                server
                    .environment()
                    .event_urls
                    .get("browser_path")
                    .expect("browser path url"),
            )
            .bearer_auth("wrong")
            .json(&serde_json::json!({ "value": "/posts/example-post" }))
            .send()
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            state
                .get_string("current_browser_path")
                .expect("get current_browser_path"),
            None
        );
        assert!(receiver.try_recv().is_err());

        let _ = std::fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn event_server_rejects_values_that_fail_pattern() {
        let mut config = base_config();
        config.workflow.insert(
            "publish_post_url".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "ok".into(),
                    style: crate::config::LogStyle::Plain,
                }],
                triggers: vec![],
            },
        );
        config.event.insert(
            "browser_path".into(),
            EventSpec {
                state_key: "current_browser_path".into(),
                workflow: "publish_post_url".into(),
                pattern: Some("^/posts/[a-z0-9-]+$".into()),
            },
        );
        let state_path = std::env::temp_dir().join(format!(
            "devloop-external-events-pattern-{}.json",
            std::process::id()
        ));
        let state = SessionState::load(state_path.clone()).expect("load state");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let server = ExternalEventServer::start(&config, state.clone(), sender)
            .await
            .expect("start external event server")
            .expect("event server");

        let client = reqwest::Client::new();
        let response = client
            .post(
                server
                    .environment()
                    .event_urls
                    .get("browser_path")
                    .expect("browser path url"),
            )
            .bearer_auth(&server.environment().token)
            .json(&serde_json::json!({ "value": "/admin" }))
            .send()
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            state
                .get_string("current_browser_path")
                .expect("get current_browser_path"),
            None
        );
        assert!(receiver.try_recv().is_err());

        let _ = std::fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn event_server_fails_loudly_when_workflow_dispatch_is_unavailable() {
        let mut config = base_config();
        config.workflow.insert(
            "publish_post_url".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "ok".into(),
                    style: crate::config::LogStyle::Plain,
                }],
                triggers: vec![],
            },
        );
        config.event.insert(
            "browser_path".into(),
            EventSpec {
                state_key: "current_browser_path".into(),
                workflow: "publish_post_url".into(),
                pattern: None,
            },
        );
        let state_path = std::env::temp_dir().join(format!(
            "devloop-external-events-dispatch-{}.json",
            std::process::id()
        ));
        let state = SessionState::load(state_path.clone()).expect("load state");
        let (sender, receiver) = mpsc::unbounded_channel();
        drop(receiver);
        let server = ExternalEventServer::start(&config, state.clone(), sender)
            .await
            .expect("start external event server")
            .expect("event server");

        let client = reqwest::Client::new();
        let response = client
            .post(
                server
                    .environment()
                    .event_urls
                    .get("browser_path")
                    .expect("browser path url"),
            )
            .bearer_auth(&server.environment().token)
            .json(&serde_json::json!({ "value": "/posts/example-post" }))
            .send()
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            state
                .get_string("current_browser_path")
                .expect("get current_browser_path")
                .as_deref(),
            Some("/posts/example-post")
        );

        let _ = std::fs::remove_file(state_path);
    }
}
