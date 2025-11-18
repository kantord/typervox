use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("capture start failed: {0}")]
    Start(String),
    #[error("capture stop failed: {0}")]
    Stop(String),
}

#[async_trait]
pub trait Capture: Send + Sync {
    async fn start(&mut self) -> Result<(), CaptureError>;
    async fn stop(&mut self) -> Result<CapturedAudio, CaptureError>;
}

#[derive(Debug, Clone)]
pub struct CapturedAudio {
    pub samples: Vec<f32>,
}

#[derive(Debug, Default)]
pub struct MockCapture {
    pub frames: Vec<i16>,
}

#[async_trait]
impl Capture for MockCapture {
    async fn start(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn stop(&mut self) -> Result<CapturedAudio, CaptureError> {
        let norm = 32768f32;
        let samples = self
            .frames
            .iter()
            .map(|s| *s as f32 / norm)
            .collect();
        Ok(CapturedAudio { samples })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_capture_converts_i16_to_f32() {
        let mut cap = MockCapture {
            frames: vec![0, i16::MAX, i16::MIN],
        };
        cap.start().await.unwrap();
        let captured = cap.stop().await.unwrap();
        assert_eq!(captured.samples.len(), 3);
        assert_eq!(captured.samples[0], 0.0);
        assert!((captured.samples[1] - 1.0).abs() < 1e-3);
        assert!((captured.samples[2] + 1.0).abs() < 1e-3);
    }
}
