use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{error, info};

use crate::config::Config;

pub type BrowserReloadSender = broadcast::Sender<String>;

#[derive(Debug, Clone)]
pub struct BrowserReloadEnvironment {
    pub events_url: String,
}

pub struct BrowserReloadServer {
    environment: BrowserReloadEnvironment,
    sender: BrowserReloadSender,
    task: tokio::task::JoinHandle<()>,
}

impl BrowserReloadServer {
    pub async fn start(config: &Config) -> Result<Option<Self>> {
        if !config.has_browser_reload_notifications() {
            return Ok(None);
        }

        let bind_addr: SocketAddr =
            config.browser_reload_server.bind.parse().with_context(|| {
                format!(
                    "invalid browser reload server bind '{}'",
                    config.browser_reload_server.bind
                )
            })?;
        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("failed to bind browser reload server at {bind_addr}"))?;
        let local_addr = listener
            .local_addr()
            .context("failed to read browser reload server address")?;
        let environment = BrowserReloadEnvironment {
            events_url: format!("http://{local_addr}/browser-events"),
        };
        let (sender, _receiver) = broadcast::channel(16);
        let app = Router::new()
            .route("/browser-events", get(browser_events))
            .with_state(Arc::new(sender.clone()));
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                error!("browser reload server stopped unexpectedly: {}", error);
            }
        });

        info!(
            "listening for browser reload events at {}",
            environment.events_url
        );
        Ok(Some(Self {
            environment,
            sender,
            task,
        }))
    }

    pub fn environment(&self) -> &BrowserReloadEnvironment {
        &self.environment
    }

    pub fn sender(&self) -> BrowserReloadSender {
        self.sender.clone()
    }
}

impl Drop for BrowserReloadServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn notify_browser_reload(sender: &BrowserReloadSender) {
    let _ = sender.send("reload".to_string());
}

pub fn apply_browser_reload_env(
    env: &mut BTreeMap<String, String>,
    runtime: Option<&BrowserReloadEnvironment>,
) {
    let Some(runtime) = runtime else {
        return;
    };

    env.insert(
        "DEVLOOP_BROWSER_EVENTS_URL".into(),
        runtime.events_url.clone(),
    );
}

async fn browser_events(
    State(sender): State<Arc<BrowserReloadSender>>,
) -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>,
) {
    let stream = BroadcastStream::new(sender.subscribe()).filter_map(|result| match result {
        Ok(message) => Some(Ok(Event::default().data(message))),
        Err(BroadcastStreamRecvError::Lagged(_)) => None,
    });

    (
        [(ACCESS_CONTROL_ALLOW_ORIGIN, "*")],
        Sse::new(stream).keep_alive(KeepAlive::default()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BrowserReloadServerConfig, Config, EventServerConfig, WorkflowSpec, WorkflowStep,
    };

    fn base_config() -> Config {
        Config {
            root: ".".into(),
            debounce_ms: 100,
            state_file: Some("./state.json".into()),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: EventServerConfig::default(),
            browser_reload_server: BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        }
    }

    #[test]
    fn apply_browser_reload_env_exports_events_url() {
        let runtime = BrowserReloadEnvironment {
            events_url: "http://127.0.0.1:12345/browser-events".into(),
        };
        let mut env = BTreeMap::new();

        apply_browser_reload_env(&mut env, Some(&runtime));

        assert_eq!(
            env.get("DEVLOOP_BROWSER_EVENTS_URL").map(String::as_str),
            Some("http://127.0.0.1:12345/browser-events")
        );
    }

    #[tokio::test]
    async fn browser_reload_server_streams_reload_events() {
        let mut config = base_config();
        config.workflow.insert(
            "rust".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::NotifyReload],
            },
        );

        let server = BrowserReloadServer::start(&config)
            .await
            .expect("start browser reload server")
            .expect("browser reload server");
        let mut response = reqwest::get(&server.environment().events_url)
            .await
            .expect("connect to sse endpoint");
        notify_browser_reload(&server.sender());

        assert_eq!(
            response
                .headers()
                .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );

        let first_chunk = response
            .chunk()
            .await
            .expect("read event chunk")
            .expect("non-empty event chunk");

        assert!(
            String::from_utf8_lossy(&first_chunk).contains("data: reload"),
            "expected SSE chunk to contain reload event, got {:?}",
            first_chunk
        );
    }
}
