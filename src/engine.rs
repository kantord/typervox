use async_trait::async_trait;
use thiserror::Error;

use crate::capture::CapturedAudio;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("decode failed: {0}")]
    Decode(String),
}

#[async_trait]
pub trait Engine: Send + Sync {
    async fn decode(&self, audio: &CapturedAudio) -> Result<EngineResult, EngineError>;
}

#[derive(Debug, Clone)]
pub struct EngineResult {
    pub text: String,
    pub decode_ms: u64,
    pub lang: String,
}

#[cfg(feature = "mock-engine")]
#[derive(Debug, Default)]
pub struct MockEngine;

#[cfg(feature = "mock-engine")]
#[async_trait]
impl Engine for MockEngine {
    async fn decode(&self, audio: &CapturedAudio) -> Result<EngineResult, EngineError> {
        Ok(EngineResult {
            text: format!("len={}", audio.samples.len()),
            decode_ms: 0,
            lang: "en".to_string(),
        })
    }
}

#[cfg(feature = "engine-ct2")]
#[derive(Debug)]
pub struct Ct2Engine;

#[cfg(feature = "engine-ct2")]
#[async_trait]
impl Engine for Ct2Engine {
    async fn decode(&self, _audio: &CapturedAudio) -> Result<EngineResult, EngineError> {
        unimplemented!("ct2 engine not wired yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_engine_reports_length() {
        let engine = MockEngine;
        let res = engine
            .decode(&CapturedAudio {
                samples: vec![0.0, 1.0],
            })
            .await
            .unwrap();
        assert_eq!(res.text, "len=2");
        assert_eq!(res.lang, "en");
    }
}
