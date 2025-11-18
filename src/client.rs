use crate::state::StatusSnapshot;
use axum::body::Body;
use http_body_util::BodyExt;
use hyper::{client::conn::http1, Request};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("connect failed: {0}")]
    Connect(std::io::Error),
    #[error("http error: {0}")]
    Http(#[from] hyper::Error),
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
}

pub async fn get_status(socket: &Path) -> Result<StatusSnapshot, ClientError> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(ClientError::Connect)?;
    let (mut sender, conn) = http1::Builder::new()
        .handshake::<_, Body>(TokioIo::new(stream))
        .await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = Request::builder()
        .method("GET")
        .uri("http://localhost/v1/status")
        .body(Body::empty())
        .unwrap();
    let resp = sender.send_request(req).await?;
    let body = resp.into_body().collect().await?.to_bytes();
    let status: StatusSnapshot = serde_json::from_slice(&body)?;
    Ok(status)
}

#[derive(Debug, Deserialize)]
pub struct StopActiveResponse {
    pub ok: bool,
    pub request_id: String,
    pub text: String,
}

pub async fn stop_active(socket: &Path, raw: bool) -> Result<String, ClientError> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(ClientError::Connect)?;
    let (mut sender, conn) = http1::Builder::new()
        .handshake::<_, Body>(TokioIo::new(stream))
        .await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let body = serde_json::json!({ "raw": raw });
    let req = Request::builder()
        .method("POST")
        .uri("http://localhost/v1/stop_active")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = sender.send_request(req).await?;
    let collected = resp.into_body().collect().await?.to_bytes();
    let output = String::from_utf8_lossy(&collected).to_string();
    Ok(output)
}

pub async fn start_stream(socket: &Path, app: &str, hint: &str) -> Result<(), ClientError> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(ClientError::Connect)?;
    let (mut sender, conn) = http1::Builder::new()
        .handshake::<_, Body>(TokioIo::new(stream))
        .await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let body = serde_json::json!({ "context": { "app": app, "hint": hint } });
    let req = Request::builder()
        .method("POST")
        .uri("http://localhost/v1/start?stream=1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let mut resp = sender.send_request(req).await?;
    while let Some(frame) = resp.body_mut().frame().await {
        let frame = frame?;
        if let Some(bytes) = frame.data_ref() {
            let chunk = String::from_utf8_lossy(bytes);
            for line in chunk.lines() {
                if let Some(rest) = line.strip_prefix("data: ") {
                    println!("{}", rest);
                }
            }
        }
    }
    Ok(())
}
