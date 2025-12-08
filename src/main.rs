use std::env;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result};
use ct2rs::{ComputeType, Config, Device, Whisper, WhisperOptions, download_model};

const DEFAULT_MODEL_ID: &str = "Systran/faster-whisper-tiny.en";
const DEFAULT_LANG: &str = "en";
const CHUNK_MS: u64 = 100;
const DECODE_INTERVAL_MS: u64 = 1_000;
const MAX_BUFFER_SECS: usize = 30;

struct Engine {
    whisper: Whisper,
    language: String,
    sample_rate: usize,
    max_samples: usize,
}

fn main() -> Result<()> {
    let engine = init_engine()?;
    let pcm = open_alsa_capture(engine.sample_rate as u32)?;
    capture_loop(pcm, engine)
}

fn init_engine() -> Result<Engine> {
    let model_id = env::var("TVX_MODEL_ID").unwrap_or_else(|_| DEFAULT_MODEL_ID.to_string());
    let language = env::var("TVX_LANG").unwrap_or_else(|_| DEFAULT_LANG.to_string());

    let model_dir =
        download_model(&model_id).with_context(|| format!("downloading model {}", model_id))?;

    let mut config = Config::default();
    config.device = Device::CPU;
    config.compute_type = ComputeType::AUTO;

    let whisper = Whisper::new(&model_dir, config)
        .with_context(|| format!("loading whisper model from {}", model_dir.display()))?;

    let sample_rate = whisper.sampling_rate();
    let max_samples = sample_rate * MAX_BUFFER_SECS;

    Ok(Engine {
        whisper,
        language,
        sample_rate,
        max_samples,
    })
}

fn open_alsa_capture(sample_rate: u32) -> Result<PCM> {
    let pcm = PCM::new("default", Direction::Capture, false)
        .context("opening ALSA capture on default device")?;

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
            transcribe_and_print(&engine, &audio_buffer, &mut last_printed_len, &options)?;
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
    options: &WhisperOptions,
) -> Result<()> {
    let segments = engine
        .whisper
        .generate(audio_buffer, Some(engine.language.as_str()), false, options)
        .context("running whisper transcription")?;

    let full_text = segments.join("");
    if full_text.len() > *last_printed_len {
        let new_part = &full_text[*last_printed_len..];
        print!("{new_part}");
        io::stdout()
            .flush()
            .context("flushing transcription output to stdout")?;
        *last_printed_len = full_text.len();
    }
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
