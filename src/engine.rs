use async_trait::async_trait;
use thiserror::Error;

use crate::capture::CapturedAudio;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("decode failed: {0}")]
    Decode(String),
}

#[async_trait]
pub trait Engine: Send + Sync + 'static {
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
pub struct Ct2Engine {
    whisper: ct2rs::Whisper,
}

#[cfg(feature = "engine-ct2")]
const DEFAULT_MODEL_ID: &str = "guillaumekln/faster-whisper-small";

#[cfg(feature = "engine-ct2")]
impl Ct2Engine {
    pub fn new_default() -> Result<Self, EngineError> {
        Self::new(DEFAULT_MODEL_ID)
    }

    pub fn new(model_id: &str) -> Result<Self, EngineError> {
        let path = ct2rs::download_model(model_id)
            .map_err(|e| EngineError::Decode(format!("model download: {e}")))?;
        let whisper = ct2rs::Whisper::new(path, ct2rs::Config::default())
            .map_err(|e| EngineError::Decode(format!("model load: {e}")))?;
        Ok(Self { whisper })
    }
}

#[cfg(feature = "engine-ct2")]
#[async_trait]
impl Engine for Ct2Engine {
    async fn decode(&self, audio: &CapturedAudio) -> Result<EngineResult, EngineError> {
        let options = ct2rs::WhisperOptions::default();
        let start = std::time::Instant::now();
        let result = self
            .whisper
            .generate(&audio.samples, None, false, &options)
            .map_err(|e| EngineError::Decode(format!("{e}")))?;
        let decode_ms = start.elapsed().as_millis() as u64;
        let text = result.into_iter().next().unwrap_or_default();
        Ok(EngineResult {
            text,
            decode_ms,
            lang: "en".to_string(),
        })
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

    #[cfg(feature = "engine-ct2")]
    #[tokio::test]
    async fn ct2_engine_compiles() {
        let engine = Ct2Engine;
        let res = engine
            .decode(&CapturedAudio {
                samples: vec![0.0, 1.0, 2.0],
            })
            .await
            .unwrap();
        assert_eq!(res.text, "len=3");
    }
}
