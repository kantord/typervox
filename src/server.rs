use crate::{
    capture::{Capture, MockCapture},
    engine::{Engine, EngineResult, MockEngine},
    state::{Context, QueueState, StatusSnapshot, Transcript},
};
use axum::{
    extract::State,
    response::sse::{Event, Sse},
    response::IntoResponse,
    routing::{get, post},
    BoxError, Json, Router,
};
use futures_util::StreamExt;
use hyper_util::{rt::TokioIo, server::conn::auto::Builder, service::TowerToHyperService};
use serde::Deserialize;
use std::{
    convert::Infallible,
    future::Future,
    time::Duration,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    collections::HashMap,
};
use thiserror::Error;
use tokio::{
    net::UnixListener,
    sync::{mpsc, Mutex},
};
use tokio_stream::{wrappers::ReceiverStream, wrappers::UnixListenerStream};

#[derive(Clone)]
pub struct AppState {
    queue: Arc<Mutex<QueueState>>,
    next_id: Arc<AtomicU64>,
    streams: Arc<Mutex<HashMap<String, mpsc::Sender<Event>>>>,
    capture: Arc<Mutex<Box<dyn Capture>>>,
    engine: Arc<dyn Engine + Send + Sync>,
    config: ServerConfig,
    timeout_handle: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
}

impl AppState {
    pub fn new(
        queue: Arc<Mutex<QueueState>>,
        capture: Box<dyn Capture>,
        engine: Arc<dyn Engine + Send + Sync>,
        config: ServerConfig,
    ) -> Self {
        Self {
            queue,
            next_id: Arc::new(AtomicU64::new(1)),
            streams: Arc::new(Mutex::new(HashMap::new())),
            capture: Arc::new(Mutex::new(capture)),
            engine,
            config,
            timeout_handle: Arc::new(Mutex::new(None)),
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

#[derive(Clone, Copy)]
pub struct ServerConfig {
    pub max_queue_len: usize,
    pub max_record_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_queue_len: 64,
            max_record_ms: 60_000,
        }
    }
}

pub fn router_with_config(
    state: Arc<Mutex<QueueState>>,
    config: ServerConfig,
) -> Router {
    Router::new()
        .route("/v1/status", get(get_status))
        .route("/v1/start", post(post_start))
        .route("/v1/stop_active", post(post_stop_active))
        .with_state(AppState::new(
            state,
            Box::<MockCapture>::default(),
            Arc::new(MockEngine),
            config,
        ))
}

pub fn router(state: Arc<Mutex<QueueState>>) -> Router {
    router_with_config(state, ServerConfig::default())
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
) -> axum::response::Response {
    let request_id = format!(
        "rq_{}",
        app_state.next_id.fetch_add(1, Ordering::Relaxed)
    );

    let StartRequest { context } = body;
    let context_for_queue = context.clone();

    let (tx, rx) = mpsc::channel::<Event>(4);
    {
        let mut streams = app_state.streams.lock().await;
        streams.insert(request_id.clone(), tx.clone());
    }

    let position = {
        let mut queue = app_state.queue.lock().await;
        if queue.len() >= app_state.config.max_queue_len {
            let payload = serde_json::json!({"error": "queue-full"});
            return axum::response::Response::builder()
                .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap();
        }
        let pos = queue.enqueue(request_id.clone(), context_for_queue, 0);
        // Only mark recording if this is the head.
        if pos == 0 {
            queue.promote();
            start_capture(&app_state).await;
            schedule_timeout(app_state.clone(), request_id.clone()).await;
        }
        pos
    };

    let queued = Event::default().event("queued").data(
        serde_json::json!({
            "request_id": request_id,
            "position": position,
            "context": context
        })
        .to_string(),
    );
    let initial_events: Vec<Result<Event, Infallible>> = if position == 0 {
        let started = Event::default().event("recording_started").data(
            serde_json::json!({
                "request_id": request_id,
            })
            .to_string(),
        );
        vec![Ok(queued), Ok(started)]
    } else {
        vec![Ok(queued)]
    };

    let event_stream = futures_util::stream::iter(initial_events)
        .chain(ReceiverStream::new(rx).map(Ok));
    Sse::new(event_stream).into_response()
}

#[derive(Debug, Deserialize)]
struct StopActiveRequest {
    #[serde(default)]
    raw: bool,
}

#[derive(Debug, serde::Serialize)]
struct StopActiveResponse {
    ok: bool,
    request_id: String,
    text: String,
    timings: Timings,
}

#[derive(Debug, serde::Serialize)]
struct Timings {
    decode_ms: u64,
}

async fn post_stop_active(
    State(app_state): State<AppState>,
    Json(body): Json<StopActiveRequest>,
) -> axum::response::Response {
    let outcome = finish_active(app_state.clone(), crate::state::StopReason::ClientStop).await;
    let Some((request_id, transcript)) = outcome else {
        let payload = serde_json::json!({"ok": false, "error": "not-recording"});
        return axum::response::Response::builder()
            .status(axum::http::StatusCode::BAD_REQUEST)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
    };

    if body.raw {
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/plain")
            .body(axum::body::Body::from(format!("{}\n", transcript.text)))
            .unwrap()
    } else {
        let json = serde_json::json!({
            "ok": true,
            "request_id": request_id,
            "text": transcript.text,
            "timings": { "decode_ms": transcript.decode_ms }
        })
        .to_string();
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(json))
            .unwrap()
    }
}

async fn start_capture(app_state: &AppState) {
    let mut capture = app_state.capture.lock().await;
    let _ = capture.start().await;
}

async fn schedule_timeout(app_state: AppState, expected_request: String) {
    let duration = Duration::from_millis(app_state.config.max_record_ms);
    let state_for_task = app_state.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(duration).await;
        let active = {
            let queue = state_for_task.queue.lock().await;
            queue.snapshot().active_request_id
        };
        if active.as_deref() == Some(expected_request.as_str()) {
            let _ = finish_active(state_for_task.clone(), crate::state::StopReason::Timeout).await;
        }
    });
    let abort = handle.abort_handle();
    let mut slot = app_state.timeout_handle.lock().await;
    if let Some(prev) = slot.replace(abort) {
        prev.abort();
    }
}

async fn finish_active(
    app_state: AppState,
    reason: crate::state::StopReason,
) -> Option<(String, Transcript)> {
    if reason != crate::state::StopReason::Timeout {
        if let Some(handle) = app_state.timeout_handle.lock().await.take() {
            handle.abort();
        }
    }

    let stopped = {
        let mut queue = app_state.queue.lock().await;
        queue.stop_active(reason)
    }?;

    let request_id = stopped.item.request_id.clone();

    let transcript = decode_mock(&app_state).await;
    if let Some(tx) = app_state.streams.lock().await.remove(&request_id) {
        let reason_str = match reason {
            crate::state::StopReason::ClientStop => "client_stop",
            crate::state::StopReason::Timeout => "timeout",
            crate::state::StopReason::Cancel => "cancel",
        };
        let _ = tx.send(
            Event::default().event("recording_stopped").data(
                serde_json::json!({
                    "request_id": request_id,
                    "reason": reason_str,
                    "captured_ms": 0
                })
                .to_string(),
            ),
        ).await;
        let transcript = decode_mock(&app_state).await;
        let _ = tx
            .send(
                Event::default().event("final").data(
                    serde_json::json!({
                        "request_id": request_id,
                        "text": transcript.text,
                        "timings": { "decode_ms": transcript.decode_ms },
                        "lang": transcript.lang
                    })
                    .to_string(),
                ),
            )
            .await;
    }

    // Promote next item if any.
    {
        let mut queue = app_state.queue.lock().await;
        let promoted = queue.promote().cloned();
        if let Some(promoted) = promoted {
            start_capture(&app_state).await;
            if let Some(next_tx) = app_state.streams.lock().await.get(&promoted.request_id) {
                let _ = next_tx
                    .send(
                        Event::default().event("recording_started").data(
                            serde_json::json!({ "request_id": promoted.request_id })
                                .to_string(),
                        ),
                    )
                    .await;
            }
        }
    }

    Some((request_id, transcript))
}

async fn decode_mock(app_state: &AppState) -> Transcript {
    let mut capture = app_state.capture.lock().await;
    let audio = capture.stop().await.unwrap_or_else(|_| crate::capture::CapturedAudio {
        samples: Vec::new(),
    });
    let engine = app_state.engine.clone();
    let res = engine
        .decode(&audio)
        .await
        .unwrap_or_else(|_| EngineResult {
            text: "".to_string(),
            decode_ms: 0,
            lang: "en".to_string(),
        });
    Transcript {
        text: res.text,
        decode_ms: res.decode_ms,
        lang: res.lang,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::QueueState;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::{client::conn::http1, Request, StatusCode};
    use hyper_util::rt::TokioIo;
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
    async fn start_streams_queue_positions_and_promotion() {
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

        let start_req = |app: &str| {
            let body = serde_json::json!({
                "context": { "app": app, "hint": "" }
            });
            Request::builder()
                .method("POST")
                .uri("http://localhost/v1/start?stream=1")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request build")
        };

        async fn start_stream(
            sock_path: &std::path::Path,
            req: Request<axum::body::Body>,
        ) -> hyper::body::Incoming {
            let stream = tokio::net::UnixStream::connect(sock_path)
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

            sender
                .send_request(req)
                .await
                .expect("send")
                .into_body()
        }

        let mut sse1 = start_stream(&sock_path, start_req("app1")).await;
        let mut sse2 = start_stream(&sock_path, start_req("app2")).await;
        let mut sse3 = start_stream(&sock_path, start_req("app3")).await;

        async fn collect_until(body: &mut hyper::body::Incoming, needle: &str) -> String {
            let mut buf = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.expect("frame");
                if let Some(bytes) = frame.data_ref() {
                    buf.push(bytes.clone());
                    if String::from_utf8_lossy(&buf.concat()).contains(needle) {
                        break;
                    }
                }
            }
            String::from_utf8(buf.concat().to_vec()).unwrap()
        }

        let first_body = collect_until(&mut sse1, "recording_started").await;
        assert!(first_body.contains("\"position\":0"));

        let second_body = collect_until(&mut sse2, "queued").await;
        assert!(second_body.contains("\"position\":1"));
        assert!(!second_body.contains("recording_started"));

        let third_body = collect_until(&mut sse3, "queued").await;
        assert!(third_body.contains("\"position\":2"));
        assert!(!third_body.contains("recording_started"));

        // Stop active to promote next.
        let stop_stream = tokio::net::UnixStream::connect(&sock_path)
            .await
            .expect("connect UDS stop");
        let (mut stop_sender, stop_conn) =
            http1::Builder::new()
                .handshake::<_, Full<Bytes>>(TokioIo::new(stop_stream))
                .await
                .expect("handshake stop");
        tokio::spawn(async move { let _ = stop_conn.await; });

        let stop_body = serde_json::json!({ "raw": false });
        let stop_req = Request::builder()
            .method("POST")
            .uri("http://localhost/v1/stop_active")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(stop_body.to_string())))
            .expect("stop req");
        let _ = stop_sender.send_request(stop_req).await.expect("stop resp");

        // sse2 should now receive recording_started
        let second_after = collect_until(&mut sse2, "recording_started").await;
        assert!(second_after.contains("recording_started"));

        let _ = shutdown_tx.send(());
        let _ = server_handle.await;
    }
}
