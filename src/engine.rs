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
    lang: Option<String>,
}

#[cfg(feature = "engine-ct2")]
const DEFAULT_MODEL_ID: &str = "Systran/faster-whisper-tiny.en";

#[cfg(feature = "engine-ct2")]
#[derive(Debug, Clone)]
pub struct Ct2Config {
    pub model_id: Option<String>,
    pub lang: Option<String>,
    pub use_cuda: bool,
    pub compute_type: Option<ct2rs::ComputeType>,
}

#[cfg(feature = "engine-ct2")]
impl Default for Ct2Config {
    fn default() -> Self {
        Self {
            model_id: Some(DEFAULT_MODEL_ID.to_string()),
            lang: Some("en".to_string()),
            use_cuda: false,
            compute_type: None,
        }
    }
}

#[cfg(feature = "engine-ct2")]
impl Ct2Engine {
    pub fn new_default() -> Result<Self, EngineError> {
        Self::new(&Ct2Config::default())
    }

    pub fn new(config: &Ct2Config) -> Result<Self, EngineError> {
        let resolved_id = config
            .model_id
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string());
        let path = ct2rs::download_model(&resolved_id)
            .map_err(|e| EngineError::Decode(format!("model download: {e}")))?;
        let mut ct2_cfg = ct2rs::Config::default();
        let compute_type = config.compute_type.unwrap_or(if config.use_cuda {
            ct2rs::ComputeType::FLOAT16
        } else {
            // DEFAULT lets CTranslate2 pick a supported type for the CPU/backend.
            ct2rs::ComputeType::DEFAULT
        });
        if config.use_cuda {
            ct2_cfg.device = ct2rs::Device::CUDA;
            ct2_cfg.device_indices = vec![0];
        }
        ct2_cfg.compute_type = compute_type;
        let whisper = ct2rs::Whisper::new(path, ct2_cfg)
            .map_err(|e| EngineError::Decode(format!("model load: {e}")))?;
        Ok(Self {
            whisper,
            lang: config.lang.clone(),
        })
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
            .generate(&audio.samples, self.lang.as_deref(), false, &options)
            .map_err(|e| EngineError::Decode(format!("{e}")))?;
        let decode_ms = start.elapsed().as_millis() as u64;
        let text = result.into_iter().next().unwrap_or_default();
        Ok(EngineResult {
            text,
            decode_ms,
            lang: self.lang.clone().unwrap_or_else(|| "unknown".to_string()),
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
}
