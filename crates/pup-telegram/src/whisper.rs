use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use audiopus::coder::Decoder as OpusDecoder;
use audiopus::{Channels, SampleRate};
use ogg::reading::PacketReader;
use tracing::{debug, info};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Default model size when none is specified.
const DEFAULT_MODEL: &str = "base";

/// Hugging Face mirror for ggml model files.
const MODEL_BASE_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Speech-to-text transcriber backed by whisper.cpp.
pub(crate) struct Transcriber {
    ctx: WhisperContext,
    language: Option<String>,
}

impl std::fmt::Debug for Transcriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcriber")
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl Transcriber {
    /// Load (or download) a whisper model and create a transcriber.
    ///
    /// `model` can be:
    /// - A full path to a `.bin` file
    /// - A model size name: `tiny`, `base`, `small`, `medium`, `large`
    ///
    /// If a size name is given and the file doesn't exist yet, it will be
    /// downloaded from Hugging Face into `cache_dir`.
    pub(crate) async fn new(
        model: Option<&str>,
        language: Option<String>,
        cache_dir: &Path,
    ) -> Result<Self> {
        let model_str = model.unwrap_or(DEFAULT_MODEL);
        let model_path = resolve_model_path(model_str, cache_dir).await?;

        info!(model = %model_path.display(), "loading whisper model");
        let ctx = WhisperContext::new_with_params(
            model_path
                .to_str()
                .context("model path is not valid UTF-8")?,
            WhisperContextParameters::default(),
        )
        .map_err(|e| anyhow::anyhow!("failed to load whisper model: {e}"))?;

        Ok(Self { ctx, language })
    }

    /// Transcribe raw 16 kHz mono f32 PCM audio to text.
    pub(crate) fn transcribe(&self, pcm: &[f32]) -> Result<String> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| anyhow::anyhow!("whisper state: {e}"))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

        // Language: None means auto-detect.
        if let Some(ref lang) = self.language
            && lang != "auto" {
                params.set_language(Some(lang));
            }

        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // Two threads is enough for short voice messages.
        params.set_n_threads(2);

        state
            .full(params, pcm)
            .map_err(|e| anyhow::anyhow!("whisper inference failed: {e}"))?;

        let n = state.full_n_segments();
        let mut text = String::new();
        for i in 0..n {
            if let Some(seg) = state.get_segment(i)
                && let Ok(s) = seg.to_str_lossy() {
                    text.push_str(&s);
                }
        }

        Ok(text.trim().to_owned())
    }
}

// ── OGG/Opus → 16 kHz mono PCM ───────────────────────────────────────

/// Decode an OGG/Opus voice message to 16 kHz mono f32 PCM.
///
/// Uses the bundled libopus (no ffmpeg required).
pub(crate) fn decode_ogg_opus(ogg_data: &[u8]) -> Result<Vec<f32>> {
    let mut reader = PacketReader::new(Cursor::new(ogg_data));

    // First packet: OpusHead header (RFC 7845 §5.1).
    let head = reader
        .read_packet()?
        .context("missing OpusHead packet")?;
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
        bail!("invalid OpusHead header");
    }
    let channels = head.data[9] as usize;
    let pre_skip =
        u16::from_le_bytes([head.data[10], head.data[11]]) as usize;

    // Second packet: OpusTags (skip).
    reader
        .read_packet()?
        .context("missing OpusTags packet")?;

    // Opus always decodes at 48 kHz.
    let ch = if channels == 1 {
        Channels::Mono
    } else {
        Channels::Stereo
    };
    let mut decoder = OpusDecoder::new(SampleRate::Hz48000, ch)
        .map_err(|e| anyhow::anyhow!("opus decoder init: {e}"))?;

    // Decode all audio packets.
    let max_frame = 5760; // 120 ms at 48 kHz (max Opus frame)
    let mut decode_buf = vec![0f32; max_frame * channels];
    let mut pcm_48k = Vec::new();

    while let Some(packet) = reader.read_packet()? {
        let n = decoder
            .decode_float(Some(&packet.data), &mut decode_buf, false)
            .map_err(|e| anyhow::anyhow!("opus decode: {e}"))?;

        if channels >= 2 {
            // Downmix to mono.
            for i in 0..n {
                let sum: f32 = (0..channels)
                    .map(|c| decode_buf[i * channels + c])
                    .sum();
                #[allow(clippy::cast_precision_loss)]
                pcm_48k.push(sum / channels as f32);
            }
        } else {
            pcm_48k.extend_from_slice(&decode_buf[..n]);
        }
    }

    // Strip pre-skip samples (encoder delay).
    let start = pre_skip.min(pcm_48k.len());
    let pcm_48k = &pcm_48k[start..];

    // Resample 48 kHz → 16 kHz (3:1 ratio, simple averaging).
    #[allow(clippy::cast_precision_loss)]
    let pcm_16k: Vec<f32> = pcm_48k
        .chunks(3)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect();

    Ok(pcm_16k)
}

// ── model resolution ──────────────────────────────────────────────────

/// Resolve a model specifier to a file path, downloading if necessary.
async fn resolve_model_path(model: &str, cache_dir: &Path) -> Result<PathBuf> {
    // If it's an existing file path, use it directly.
    let as_path = PathBuf::from(model);
    if as_path.is_file() {
        return Ok(as_path);
    }

    // Treat it as a size name (tiny, base, small, medium, large).
    let filename = model_filename(model);
    let cached = cache_dir.join(&filename);

    if cached.is_file() {
        debug!(path = %cached.display(), "using cached whisper model");
        return Ok(cached);
    }

    // Download.
    let url = format!("{MODEL_BASE_URL}/{filename}");
    info!(url, dest = %cached.display(), "downloading whisper model");

    tokio::fs::create_dir_all(cache_dir)
        .await
        .with_context(|| format!("create cache dir {}", cache_dir.display()))?;

    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {url}"))?;

    if !resp.status().is_success() {
        bail!(
            "failed to download model from {url}: HTTP {}",
            resp.status()
        );
    }

    let bytes = resp.bytes().await?;

    // Write to a temp file then rename for atomicity.
    let tmp = cached.with_extension("tmp");
    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, &cached)
        .await
        .with_context(|| format!("rename to {}", cached.display()))?;

    info!(
        path = %cached.display(),
        size_mb = bytes.len() / (1024 * 1024),
        "whisper model downloaded"
    );

    Ok(cached)
}

/// Map a short model name to its ggml filename.
fn model_filename(name: &str) -> String {
    match name {
        "tiny" | "base" | "small" | "medium" | "large" => {
            format!("ggml-{name}.bin")
        }
        // Allow e.g. "large-v3"
        other => format!("ggml-{other}.bin"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_filename() {
        assert_eq!(model_filename("tiny"), "ggml-tiny.bin");
        assert_eq!(model_filename("base"), "ggml-base.bin");
        assert_eq!(model_filename("large-v3"), "ggml-large-v3.bin");
    }
}
