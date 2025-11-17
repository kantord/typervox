use crate::state::{Context, QueueState, StatusSnapshot};
use axum::{
    extract::State,
    response::sse::{Event, Sse},
    routing::{get, post},
    BoxError, Json, Router,
};
use futures_util::StreamExt;
use hyper_util::{rt::TokioIo, server::conn::auto::Builder, service::TowerToHyperService};
use serde::Deserialize;
use std::{
    convert::Infallible,
    future::Future,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use thiserror::Error;
use tokio::{net::UnixListener, sync::Mutex};
use tokio_stream::wrappers::UnixListenerStream;

#[derive(Clone)]
pub struct AppState {
    queue: Arc<Mutex<QueueState>>,
    next_id: Arc<AtomicU64>,
}

impl AppState {
    pub fn new(queue: Arc<Mutex<QueueState>>) -> Self {
        Self {
            queue,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to bind unix socket: {0}")]
    Bind(#[from] std::io::Error),
    #[error("serve error: {0}")]
    Serve(#[from] BoxError),
}

pub fn router(state: Arc<Mutex<QueueState>>) -> Router {
    Router::new()
        .route("/v1/status", get(get_status))
        .route("/v1/start", post(post_start))
        .with_state(AppState::new(state))
}

pub async fn serve_unix<P, F>(
    socket_path: P,
    router: Router,
    shutdown: F,
) -> Result<(), ServerError>
where
    P: AsRef<Path>,
    F: Future<Output = ()> + Send + 'static,
{
    let path = socket_path.as_ref();
    if path.exists() {
        std::fs::remove_file(path)?;
    }

    let uds = UnixListener::bind(path)?;
    let mut incoming = UnixListenerStream::new(uds);
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            maybe_stream = incoming.next() => {
                let Some(stream_result) = maybe_stream else { break; };
                let Ok(stream) = stream_result else { continue; };
                let service = router.clone();
                tokio::spawn(async move {
                    let stream = TokioIo::new(stream);
                    let hyper_service = TowerToHyperService::new(service);
                    let _ = Builder::new(hyper_util::rt::TokioExecutor::new())
                        .serve_connection_with_upgrades(stream, hyper_service)
                        .await;
                });
            }
        }
    }

    Ok(())
}

async fn get_status(State(app_state): State<AppState>) -> Json<StatusSnapshot> {
    let queue = app_state.queue.lock().await;
    Json(queue.snapshot())
}

#[derive(Debug, Deserialize)]
struct StartRequest {
    context: Context,
}

async fn post_start(
    State(app_state): State<AppState>,
    Json(body): Json<StartRequest>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let request_id = format!(
        "rq_{}",
        app_state.next_id.fetch_add(1, Ordering::Relaxed)
    );

    let StartRequest { context } = body;
    let context_for_queue = context.clone();

    {
        let mut queue = app_state.queue.lock().await;
        queue.enqueue(request_id.clone(), context_for_queue, 0);
        queue.promote();
    }

    let queued = Event::default().event("queued").data(
        serde_json::json!({
            "request_id": request_id,
            "position": 0,
            "context": context
        })
        .to_string(),
    );
    let started = Event::default().event("recording_started").data(
        serde_json::json!({
            "request_id": request_id,
        })
        .to_string(),
    );

    let stream = futures_util::stream::iter(vec![Ok(queued), Ok(started)]);
    Sse::new(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::QueueState;
    use http_body_util::BodyExt;
    use hyper::{client::conn::http1, Request, StatusCode};
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn status_returns_empty_queue_via_unix_socket() {
        let tmpdir = tempfile::tempdir().expect("temp dir");
        let sock_path = tmpdir.path().join("typervox.sock");
        let state = Arc::new(Mutex::new(QueueState::default()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let router = router(state.clone());

        let server_handle = tokio::spawn({
            let socket_path = sock_path.clone();
            async move {
                serve_unix(
                    socket_path,
                    router,
                    async { let _ = shutdown_rx.await; },
                )
                .await
                .unwrap();
            }
        });

        // Allow the server a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stream = tokio::net::UnixStream::connect(&sock_path)
            .await
            .expect("connect UDS");
        let (mut sender, connection) =
            http1::Builder::new()
                .handshake::<_, axum::body::Body>(TokioIo::new(stream))
                .await
                .expect("handshake");
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let request = Request::builder()
            .method("GET")
            .uri("http://localhost/v1/status")
            .body(axum::body::Body::empty())
            .expect("request build");

        let response = sender
            .send_request(request)
            .await
            .expect("send request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("json body");
        let expected = serde_json::json!({
            "ok": true,
            "active_request_id": null,
            "queue": []
        });
        assert_eq!(json, expected);

        let _ = shutdown_tx.send(());
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn start_streams_queued_then_recording_started() {
        let tmpdir = tempfile::tempdir().expect("temp dir");
        let sock_path = tmpdir.path().join("typervox.sock");
        let state = Arc::new(Mutex::new(QueueState::default()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let router = router(state.clone());

        let server_handle = tokio::spawn({
            let socket_path = sock_path.clone();
            async move {
                serve_unix(
                    socket_path,
                    router,
                    async { let _ = shutdown_rx.await; },
                )
                .await
                .unwrap();
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let stream = tokio::net::UnixStream::connect(&sock_path)
            .await
            .expect("connect UDS");
        let (mut sender, connection) =
            http1::Builder::new()
                .handshake::<_, axum::body::Body>(TokioIo::new(stream))
                .await
                .expect("handshake");
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let body = serde_json::json!({
            "context": { "app": "chrome", "hint": "omnibox" }
        });
        let request = Request::builder()
            .method("POST")
            .uri("http://localhost/v1/start?stream=1")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .expect("request build");

        let response = sender
            .send_request(request)
            .await
            .expect("send request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        let body_str = String::from_utf8(body.to_vec()).expect("utf8 sse");
        let mut events = body_str.split("\n\n");
        let first = events.next().expect("first").trim();
        let second = events.next().expect("second").trim();

        assert!(first.starts_with("event: queued"));
        assert!(second.starts_with("event: recording_started"));

        let parse_data = |chunk: &str| {
            chunk
                .lines()
                .find(|line| line.starts_with("data: "))
                .map(|line| &line[6..])
                .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
                .unwrap()
        };

        let first_data = parse_data(first);
        let second_data = parse_data(second);
        let request_id = first_data["request_id"].as_str().unwrap();
        assert_eq!(first_data["position"], serde_json::json!(0));
        assert_eq!(
            first_data["context"],
            serde_json::json!({
                "app": "chrome",
                "hint": "omnibox"
            })
        );
        assert_eq!(second_data["request_id"], request_id);

        let _ = shutdown_tx.send(());
        let _ = server_handle.await;
    }
}
