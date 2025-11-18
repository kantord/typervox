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
}

impl AppState {
    pub fn new(queue: Arc<Mutex<QueueState>>) -> Self {
        Self {
            queue,
            next_id: Arc::new(AtomicU64::new(1)),
            streams: Arc::new(Mutex::new(HashMap::new())),
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
        .route("/v1/stop_active", post(post_stop_active))
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

    let (tx, rx) = mpsc::channel::<Event>(4);
    {
        let mut streams = app_state.streams.lock().await;
        streams.insert(request_id.clone(), tx.clone());
    }

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

    let initial = futures_util::stream::iter(vec![Ok(queued), Ok(started)]);
    let event_stream = initial.chain(ReceiverStream::new(rx).map(Ok));
    Sse::new(event_stream)
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
    let stopped = {
        let mut queue = app_state.queue.lock().await;
        queue.stop_active(crate::state::StopReason::ClientStop)
    };

    let Some(stopped) = stopped else {
        let payload = serde_json::json!({"ok": false, "error": "not-recording"});
        return axum::response::Response::builder()
            .status(axum::http::StatusCode::BAD_REQUEST)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
    };

    let request_id = stopped.item.request_id.clone();

    if let Some(tx) = app_state.streams.lock().await.remove(&request_id) {
        let _ = tx.send(
            Event::default().event("recording_stopped").data(
                serde_json::json!({
                    "request_id": request_id,
                    "reason": "client_stop",
                    "captured_ms": 0
                })
                .to_string(),
            ),
        ).await;
        let _ = tx.send(
            Event::default().event("final").data(
                serde_json::json!({
                    "request_id": request_id,
                    "text": "MOCK",
                    "timings": { "decode_ms": 0 },
                    "lang": "en"
                })
                .to_string(),
            ),
        ).await;
    }

    let response = StopActiveResponse {
        ok: true,
        request_id,
        text: "MOCK".to_string(),
        timings: Timings { decode_ms: 0 },
    };

    if body.raw {
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/plain")
            .body(axum::body::Body::from(format!("{}\n", response.text)))
            .unwrap()
    } else {
        let json = serde_json::to_string(&response).expect("serialize response");
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(json))
            .unwrap()
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
        let mut body_stream = response.into_body();

        // Spawn stop_active in parallel after we consumed first two events.
        let mut chunks = Vec::new();
        while let Some(frame) = body_stream.frame().await {
            let frame = frame.expect("frame");
            if let Some(bytes) = frame.data_ref() {
                chunks.push(bytes.clone());
                let collected = chunks.concat();
                let body_str = String::from_utf8(collected.to_vec()).expect("utf8");
                if body_str.contains("recording_started") {
                    break;
                }
            }
        }

        // Call stop_active
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
        let stop_resp = stop_sender.send_request(stop_req).await.expect("stop resp");
        assert_eq!(stop_resp.status(), StatusCode::OK);
        let stop_json: serde_json::Value = serde_json::from_slice(
            &stop_resp
                .into_body()
                .collect()
                .await
                .expect("collect stop body")
                .to_bytes(),
        )
        .expect("json stop");
        assert_eq!(stop_json["ok"], serde_json::json!(true));
        assert_eq!(stop_json["text"], serde_json::json!("MOCK"));

        // Read remaining SSE events until stream closes.
        while let Some(frame) = body_stream.frame().await {
            let frame = frame.expect("frame");
            if let Some(bytes) = frame.data_ref() {
                chunks.push(bytes.clone());
            }
        }
        let body_str = String::from_utf8(chunks.concat().to_vec()).expect("utf8 sse");
        let mut events = body_str.trim_end().split("\n\n");
        let first = events.next().expect("first").trim();
        let second = events.next().expect("second").trim();
        let third = events.next().expect("third").trim();
        let fourth = events.next().expect("fourth").trim();

        assert!(first.starts_with("event: queued"));
        assert!(second.starts_with("event: recording_started"));
        assert!(third.starts_with("event: recording_stopped"));
        assert!(fourth.starts_with("event: final"));

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
        let third_data = parse_data(third);
        let fourth_data = parse_data(fourth);
        let request_id = first_data["request_id"].as_str().unwrap();
        assert_eq!(second_data["request_id"], request_id);
        assert_eq!(third_data["request_id"], request_id);
        assert_eq!(third_data["reason"], serde_json::json!("client_stop"));
        assert_eq!(fourth_data["request_id"], request_id);
        assert_eq!(fourth_data["text"], serde_json::json!("MOCK"));

        let _ = shutdown_tx.send(());
        let _ = server_handle.await;
    }
}
