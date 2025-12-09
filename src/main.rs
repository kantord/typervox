use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result, anyhow};
use ct2rs::sys::get_device_count;
use ct2rs::{ComputeType, Config, Device, Whisper, WhisperOptions, download_model};

// Default to a lightweight model for low-latency CPU decoding.
const DEFAULT_MODEL_ID: &str = "Systran/faster-whisper-base.en";
const DEFAULT_LANG: &str = "en";
const CHUNK_MS: u64 = 40;
const DECODE_INTERVAL_MS: u64 = 150;
// Keep a short rolling buffer to keep decode latency low.
const MAX_BUFFER_SECS: usize = 3;
const DECODE_SLICE_SECS: usize = 2;

struct Engine {
    whisper: Whisper,
    language: String,
    sample_rate: usize,
    max_samples: usize,
    device: Device,
    compute_type: ComputeType,
    model_id: String,
    model_dir: PathBuf,
}

fn main() -> Result<()> {
    let engine = init_engine()?;
    eprintln!(
        "typervox config: device={:?}, compute_type={:?}, model_id={}, model_path={}, lang={}, sample_rate={}Hz",
        engine.device,
        engine.compute_type,
        engine.model_id,
        engine.model_dir.display(),
        engine.language,
        engine.sample_rate
    );
    let pcm = open_alsa_capture(engine.sample_rate as u32)?;
    capture_loop(pcm, engine)
}

fn init_engine() -> Result<Engine> {
    let model_id = env::var("TVX_MODEL_ID").unwrap_or_else(|_| DEFAULT_MODEL_ID.to_string());
    let language = env::var("TVX_LANG").unwrap_or_else(|_| DEFAULT_LANG.to_string());

    let model_dir =
        download_model(&model_id).with_context(|| format!("downloading model {}", model_id))?;

    ensure_preprocessor_config(&model_dir)?;

    let compute_type = ComputeType::INT8;
    let mut last_err: Option<anyhow::Error> = None;

    for device in device_candidates() {
        let mut config = Config::default();
        config.device = device;
        config.compute_type = compute_type;

        match Whisper::new(&model_dir, config) {
            Ok(whisper) => {
                let sample_rate = whisper.sampling_rate();
                let max_samples = sample_rate * MAX_BUFFER_SECS;
                return Ok(Engine {
                    whisper,
                    language,
                    sample_rate,
                    max_samples,
                    device,
                    compute_type,
                    model_id,
                    model_dir,
                });
            }
            Err(err) => {
                eprintln!("failed to init whisper on {:?}: {:#}", device, err);
                last_err = Some(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("failed to initialize whisper on any device")))
}

fn device_candidates() -> Vec<Device> {
    if get_device_count(Device::CUDA) > 0 {
        vec![Device::CUDA, Device::CPU]
    } else {
        vec![Device::CPU]
    }
}

fn ensure_preprocessor_config(model_dir: &Path) -> Result<()> {
    let path: PathBuf = model_dir.join("preprocessor_config.json");
    if path.exists() {
        return Ok(());
    }

    // Whisper defaults compatible with 16 kHz models such as faster-whisper.
    let json = r#"{
  "chunk_length": 30,
  "feature_extractor_type": "WhisperFeatureExtractor",
  "feature_size": 80,
  "hop_length": 160,
  "n_fft": 400,
  "n_samples": 480000,
  "nb_max_frames": 3000,
  "padding_side": "right",
  "padding_value": 0.0,
  "processor_class": "WhisperProcessor",
  "return_attention_mask": false,
  "sampling_rate": 16000
}"#;

    fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn open_alsa_capture(sample_rate: u32) -> Result<PCM> {
    let device_name = "default";
    let pcm = PCM::new(device_name, Direction::Capture, false)
        .context("opening ALSA capture on default device")?;
    if let Ok(info) = pcm.info() {
        eprintln!(
            "capturing from ALSA '{}': card={}, device={}, name={:?}",
            device_name,
            info.get_card(),
            info.get_device(),
            info.get_name().unwrap_or("unknown")
        );
    }

    {
        let hwp = HwParams::any(&pcm).context("creating ALSA HW params")?;
        hwp.set_channels(1).context("setting mono capture")?;
        hwp.set_rate_resample(true).context("enabling resampling")?;
        hwp.set_rate(sample_rate, ValueOr::Nearest)
            .context("setting sample rate")?;
        hwp.set_format(Format::S16LE)
            .context("setting sample format S16LE")?;
        hwp.set_access(Access::RWInterleaved)
            .context("setting interleaved access")?;
        pcm.hw_params(&hwp).context("applying ALSA HW params")?;
    }

    pcm.prepare().context("preparing ALSA PCM")?;
    Ok(pcm)
}

fn capture_loop(pcm: PCM, engine: Engine) -> Result<()> {
    let io = pcm.io_i16().context("opening PCM reader")?;
    let mut audio_buffer: Vec<f32> = Vec::new();
    let mut last_printed_len = 0usize;
    let mut last_decode = Instant::now();
    let chunk_len = ((engine.sample_rate as u64 * CHUNK_MS) / 1_000).max(1) as usize;

    let mut options = WhisperOptions::default();
    options.beam_size = 1;
    options.max_length = 64; // keep decoding very short for low latency
    let mut last_output = String::new();

    loop {
        let mut chunk = vec![0i16; chunk_len];
        let frames_read = match io.readi(&mut chunk) {
            Ok(frames) => frames,
            Err(err) => {
                pcm.try_recover(err, true)
                    .context("recovering from ALSA read error")?;
                continue;
            }
        };

        let samples = i16_to_f32(&chunk[..frames_read]);
        audio_buffer.extend_from_slice(&samples);

        if last_decode.elapsed() >= Duration::from_millis(DECODE_INTERVAL_MS)
            && audio_buffer.len() >= engine.sample_rate / 2
        {
            let slice_samples = (DECODE_SLICE_SECS * engine.sample_rate).min(audio_buffer.len());
            let decode_buf = &audio_buffer[audio_buffer.len() - slice_samples..];

            transcribe_and_print(
                &engine,
                decode_buf,
                &mut last_printed_len,
                &mut last_output,
                &options,
            )?;
            last_decode = Instant::now();

            if audio_buffer.len() > engine.max_samples {
                let drop_len = audio_buffer.len() - engine.max_samples;
                audio_buffer.drain(..drop_len);
                last_printed_len = 0;
            }
        }
    }
}

fn transcribe_and_print(
    engine: &Engine,
    audio_buffer: &[f32],
    last_printed_len: &mut usize,
    last_output: &mut String,
    options: &WhisperOptions,
) -> Result<()> {
    let segments = engine
        .whisper
        .generate(audio_buffer, Some(engine.language.as_str()), false, options)
        .context("running whisper transcription")?;

    let full_text = segments.join("");
    let mut common = 0usize;
    for (a, b) in full_text.chars().zip(last_output.chars()) {
        if a == b {
            common += 1;
        } else {
            break;
        }
    }
    let new_tail: String = full_text.chars().skip(common).collect();
    if !new_tail.is_empty() {
        print!("{new_tail}");
        io::stdout()
            .flush()
            .context("flushing transcription output to stdout")?;
    }
    *last_printed_len = full_text.len();
    *last_output = full_text;
    Ok(())
}

fn i16_to_f32(input: &[i16]) -> Vec<f32> {
    const SCALE: f32 = 32768.0;
    input.iter().map(|s| *s as f32 / SCALE).collect()
}

#[cfg(test)]
mod tests {
    use super::i16_to_f32;

    #[test]
    fn converts_i16_to_f32() {
        let samples = vec![i16::MIN, 0, i16::MAX];
        let converted = i16_to_f32(&samples);

        assert!((converted[1] - 0.0).abs() < 1e-6);
        assert!((converted[0] + 1.0).abs() < 1e-6);
        assert!(converted[2] < 1.0);
    }
}
