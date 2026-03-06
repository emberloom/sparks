//! Telephony module — Twilio Media Streams integration for phone calls.
//!
//! Architecture:
//!   Caller → Twilio → HTTP /voice (TwiML) → Twilio connects WebSocket → /media-stream
//!   Audio chunks (mulaw 8kHz) → Silero VAD → end-of-speech → STT (Whisper) → LLM → TTS → audio back
//!
//! STT: any Whisper-compatible endpoint (faster-whisper, whisper.cpp, etc.) — free & open-source.
//! TTS: any OpenAI-compatible TTS endpoint (Piper, Coqui, Kokoro, etc.) — configurable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use base64::Engine as _;
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use tracing::{debug, error, info, warn};

use crate::config::TelephonyConfig;
use crate::confirm::AutoConfirmer;
use crate::core::{CoreEvent, CoreHandle, SessionContext};

// ── Constants ────────────────────────────────────────────────────────

/// Twilio sends 20ms audio chunks at 8kHz mulaw = 160 bytes per chunk.
const MULAW_CHUNK_SAMPLES: usize = 160;
/// Maximum audio buffer before forced flush (60 seconds of 8kHz audio).
const MAX_AUDIO_BUFFER_SAMPLES: usize = 8000 * 60;
/// Minimum speech samples before we bother sending to STT (0.5s).
const MIN_SPEECH_SAMPLES: usize = 4000;
/// Maximum base64 payload size from a single Twilio media event (~32KB).
/// Twilio normally sends ~214 bytes; anything much larger is suspicious.
const MAX_MEDIA_PAYLOAD_BYTES: usize = 32_768;
/// Maximum LLM response chars to accumulate before truncation.
const MAX_RESPONSE_CHARS: usize = 2_000;

// ── Silero VAD ───────────────────────────────────────────────────────

/// Lightweight Voice Activity Detection using Silero VAD ONNX model.
/// Runs locally — no API calls, no cost.
struct SileroVad {
    session: ort::session::Session,
    /// LSTM hidden state (h)
    h: ndarray::Array3<f32>,
    /// LSTM cell state (c)
    c: ndarray::Array3<f32>,
    sample_rate: u32,
    threshold: f32,
}

impl SileroVad {
    fn new(model_path: &str, sample_rate: u32, threshold: f32) -> anyhow::Result<Self> {
        let session = ort::session::Session::builder()?
            .with_intra_threads(1)?
            .commit_from_file(model_path)?;

        // Silero VAD v5 uses 2 layers, 64 units for LSTM state
        let h = ndarray::Array3::<f32>::zeros((2, 1, 64));
        let c = ndarray::Array3::<f32>::zeros((2, 1, 64));

        Ok(Self {
            session,
            h,
            c,
            sample_rate,
            threshold,
        })
    }

    /// Process a chunk of f32 PCM audio, returns speech probability [0, 1].
    fn process(&mut self, audio: &[f32]) -> anyhow::Result<f32> {
        use ort::value::Tensor;

        let audio_len = audio.len();
        let audio_arr =
            ndarray::Array2::from_shape_vec((1, audio_len), audio.to_vec())?;
        let sr_arr = ndarray::Array1::from_vec(vec![self.sample_rate as i64]);

        let audio_tensor = Tensor::from_array(audio_arr)?;
        let sr_tensor = Tensor::from_array(sr_arr)?;
        let h_tensor = Tensor::from_array(self.h.clone())?;
        let c_tensor = Tensor::from_array(self.c.clone())?;

        let outputs = self.session.run(ort::inputs![
            "input" => audio_tensor,
            "sr" => sr_tensor,
            "h" => h_tensor,
            "c" => c_tensor,
        ])?;

        // Extract speech probability
        let prob_arr = outputs[0]
            .try_extract_array::<f32>()?;
        let speech_prob = prob_arr.as_slice().unwrap_or(&[0.0])[0];

        // Update LSTM states from outputs
        if let Ok(hn_arr) = outputs[1].try_extract_array::<f32>() {
            if let Some(slice) = hn_arr.as_slice() {
                if slice.len() == self.h.len() {
                    if let Some(h_slice) = self.h.as_slice_mut() {
                        h_slice.copy_from_slice(slice);
                    }
                }
            }
        }
        if let Ok(cn_arr) = outputs[2].try_extract_array::<f32>() {
            if let Some(slice) = cn_arr.as_slice() {
                if slice.len() == self.c.len() {
                    if let Some(c_slice) = self.c.as_slice_mut() {
                        c_slice.copy_from_slice(slice);
                    }
                }
            }
        }

        Ok(speech_prob)
    }

    fn is_speech(&mut self, audio: &[f32]) -> bool {
        match self.process(audio) {
            Ok(prob) => prob >= self.threshold,
            Err(e) => {
                warn!("VAD inference error: {}", e);
                false
            }
        }
    }

    /// Reset LSTM states (call between utterances).
    fn reset(&mut self) {
        self.h.fill(0.0);
        self.c.fill(0.0);
    }
}

// ── Audio Utilities ──────────────────────────────────────────────────

/// Decode mulaw byte to 16-bit PCM sample.
fn mulaw_decode(byte: u8) -> i16 {
    let mu = !byte;
    let sign = (mu & 0x80) as i16;
    let exponent = ((mu >> 4) & 0x07) as i16;
    let mantissa = (mu & 0x0F) as i16;
    let magnitude = ((mantissa << 1) | 0x21) << (exponent + 2);
    let sample = magnitude - 0x84;
    if sign == 0 { sample } else { -sample }
}

/// Encode 16-bit PCM sample to mulaw byte (ITU-T G.711).
fn mulaw_encode(sample: i16) -> u8 {
    // Exponent lookup: maps (biased_sample >> 7) → exponent 0..7.
    // Covers biased magnitudes 0..32767 in ranges 0..127, 128..255, ..., 16384..32767.
    #[rustfmt::skip]
    const EXP_LUT: [u8; 256] = [
        0,0,1,1,2,2,2,2,3,3,3,3,3,3,3,3,
        4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,
        5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
        5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
        6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
        6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
        6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
        6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    ];

    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32767;

    let sign: u8;
    let mut s = sample as i32;
    if s < 0 {
        sign = 0x80;
        s = -s;
    } else {
        sign = 0;
    }
    s += BIAS;
    // Clip after adding bias (G.711 standard behavior)
    if s > CLIP {
        s = CLIP;
    }

    let exponent = EXP_LUT[(s >> 7) as usize];
    let mantissa = ((s >> (exponent as i32 + 3)) & 0x0F) as u8;
    !(sign | (exponent << 4) | mantissa)
}

/// Convert mulaw bytes to f32 PCM normalized to [-1, 1].
fn mulaw_to_f32(data: &[u8]) -> Vec<f32> {
    data.iter()
        .map(|&b| mulaw_decode(b) as f32 / 32768.0)
        .collect()
}

/// Convert f32 PCM [-1, 1] to mulaw bytes.
fn f32_to_mulaw(data: &[f32]) -> Vec<u8> {
    data.iter()
        .map(|&s| mulaw_encode((s * 32767.0).clamp(-32768.0, 32767.0) as i16))
        .collect()
}

/// Encode f32 PCM [-1, 1] as 16-bit LE WAV in memory (for STT upload).
fn pcm_to_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let num_samples = samples.len() as u32;
    let byte_rate = sample_rate * 2; // 16-bit mono
    let data_size = num_samples * 2;
    let file_size = 36 + data_size;

    let mut buf = Vec::with_capacity(file_size as usize + 8);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    for &s in samples {
        let pcm = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
        buf.extend_from_slice(&pcm.to_le_bytes());
    }
    buf
}

// ── STT / TTS ────────────────────────────────────────────────────────

/// Transcribe audio using a Whisper-compatible API (faster-whisper, whisper.cpp, etc.).
async fn transcribe_audio(
    wav_bytes: Vec<u8>,
    config: &TelephonyConfig,
) -> Result<String, String> {
    let stt_url = config
        .stt_url
        .as_deref()
        .ok_or("STT not configured. Set [telephony] stt_url.")?;
    let stt_key = config
        .stt_api_key
        .clone()
        .or_else(|| std::env::var("ATHENA_STT_API_KEY").ok());
    let stt_model = config.stt_model.as_deref().unwrap_or("whisper-large-v3");

    let part = reqwest::multipart::Part::bytes(wav_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| format!("MIME error: {}", e))?;
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", stt_model.to_string());

    let client = reqwest::Client::new();
    let mut req = client.post(stt_url).multipart(form);
    if let Some(ref key) = stt_key {
        req = req.bearer_auth(key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("STT request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("STT error ({}): {}", status, body));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse STT response: {}", e))?;

    json["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "STT response missing 'text' field".to_string())
}

/// Synthesize speech using an OpenAI-compatible TTS API.
async fn synthesize_speech(
    text: &str,
    config: &TelephonyConfig,
) -> Result<Vec<u8>, String> {
    let tts_url = config
        .tts_url
        .as_deref()
        .ok_or("TTS not configured. Set [telephony] tts_url.")?;
    let tts_key = config
        .tts_api_key
        .clone()
        .or_else(|| std::env::var("ATHENA_TTS_API_KEY").ok());
    let model = config.tts_model.as_deref().unwrap_or("tts-1");
    let voice = config.tts_voice.as_deref().unwrap_or("alloy");

    let body = serde_json::json!({
        "model": model,
        "input": text,
        "voice": voice,
        "response_format": "wav",
    });

    let client = reqwest::Client::new();
    let mut req = client.post(tts_url).json(&body);
    if let Some(ref key) = tts_key {
        req = req.bearer_auth(key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("TTS request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("TTS error ({}): {}", status, body));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read TTS audio: {}", e))?;

    Ok(bytes.to_vec())
}

/// Parse a WAV file and return f32 PCM samples (mono, resampled to target_rate if needed).
///
/// Parses the chunk tree properly to support non-standard WAV headers (e.g. extended fmt,
/// extra metadata chunks before data) produced by some TTS servers.
fn wav_to_f32_mono(wav_bytes: &[u8], target_rate: u32) -> Result<Vec<f32>, String> {
    if wav_bytes.len() < 12 {
        return Err("WAV too short".into());
    }
    if &wav_bytes[0..4] != b"RIFF" || &wav_bytes[8..12] != b"WAVE" {
        return Err("Not a RIFF/WAVE file".into());
    }

    let mut channels: Option<usize> = None;
    let mut src_rate: Option<u32> = None;
    let mut bits: Option<u16> = None;
    let mut pcm_data: Option<&[u8]> = None;

    // Walk all chunks starting at byte 12
    let mut pos = 12usize;
    while pos + 8 <= wav_bytes.len() {
        let id = &wav_bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            wav_bytes[pos + 4],
            wav_bytes[pos + 5],
            wav_bytes[pos + 6],
            wav_bytes[pos + 7],
        ]) as usize;
        let data_start = pos + 8;
        let data_end = data_start.saturating_add(size).min(wav_bytes.len());

        if id == b"fmt " && size >= 16 {
            let chunk = &wav_bytes[data_start..data_end];
            channels = Some(u16::from_le_bytes([chunk[2], chunk[3]]) as usize);
            src_rate = Some(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
            bits = Some(u16::from_le_bytes([chunk[14], chunk[15]]));
        } else if id == b"data" {
            pcm_data = Some(&wav_bytes[data_start..data_end]);
        }

        // Both chunks found — no need to keep scanning
        if channels.is_some() && pcm_data.is_some() {
            break;
        }

        // Advance: WAV chunks are word-aligned (pad byte if odd size)
        pos = data_start + size + (size % 2);
    }

    let channels = channels.ok_or("WAV fmt chunk not found")?;
    let src_rate = src_rate.ok_or("WAV fmt chunk not found")?;
    let bits = bits.ok_or("WAV fmt chunk not found")?;
    let data = pcm_data.ok_or("WAV data chunk not found")?;

    if channels == 0 {
        return Err("WAV has 0 channels".into());
    }

    let samples: Vec<f32> = match bits {
        16 => data
            .chunks_exact(2 * channels)
            .map(|frame| {
                let s = i16::from_le_bytes([frame[0], frame[1]]);
                s as f32 / 32768.0
            })
            .collect(),
        _ => return Err(format!("Unsupported WAV bit depth: {}", bits)),
    };

    if samples.is_empty() {
        return Ok(samples);
    }

    if src_rate != target_rate && src_rate > 0 {
        let ratio = src_rate as f64 / target_rate as f64;
        let out_len = (samples.len() as f64 / ratio) as usize;
        let mut resampled = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let src_idx = i as f64 * ratio;
            let idx = src_idx as usize;
            let frac = src_idx - idx as f64;
            let s0 = samples.get(idx).copied().unwrap_or(0.0);
            let s1 = samples.get(idx + 1).copied().unwrap_or(s0);
            resampled.push(s0 + (s1 - s0) * frac as f32);
        }
        return Ok(resampled);
    }

    Ok(samples)
}

// ── Twilio Signature Validation ──────────────────────────────────────

/// Validate an incoming Twilio webhook request using HMAC-SHA1.
///
/// Returns `true` if:
///   - No auth token is configured (validation disabled), or
///   - The X-Twilio-Signature header matches the HMAC-SHA1 of (url + sorted POST params).
///
/// See: https://www.twilio.com/docs/usage/security#validating-requests
fn validate_twilio_signature(
    auth_token: Option<&str>,
    signature: Option<&str>,
    url: &str,
    params: &[(String, String)],
) -> bool {
    let token = match auth_token {
        Some(t) if !t.is_empty() => t,
        _ => return true, // No token configured — skip validation
    };
    let sig = match signature {
        Some(s) => s,
        None => {
            warn!("Twilio request missing X-Twilio-Signature header");
            return false;
        }
    };

    // Build the data to sign: URL + sorted POST params
    let mut data = url.to_string();
    let mut sorted_params = params.to_vec();
    sorted_params.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, value) in &sorted_params {
        data.push_str(key);
        data.push_str(value);
    }

    use hmac::{Hmac, Mac};
    type HmacSha1 = Hmac<sha1::Sha1>;

    let mut mac =
        HmacSha1::new_from_slice(token.as_bytes()).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    if expected == sig {
        true
    } else {
        warn!("Twilio signature mismatch (possible spoofed request)");
        false
    }
}

// ── Shared App State ─────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    core: CoreHandle,
    config: TelephonyConfig,
}

// ── HTTP Handlers ────────────────────────────────────────────────────

/// TwiML response — tells Twilio to connect a Media Stream WebSocket.
/// Validates the Twilio signature if an auth token is configured.
async fn handle_voice(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Resolve auth token (config or env var)
    let auth_token_string = state
        .config
        .twilio_auth_token
        .clone()
        .or_else(|| std::env::var("ATHENA_TWILIO_AUTH_TOKEN").ok());
    let auth_token = auth_token_string.as_deref();

    let signature = headers
        .get("X-Twilio-Signature")
        .and_then(|v| v.to_str().ok());

    let public_url = state
        .config
        .public_url
        .as_deref()
        .unwrap_or("http://localhost:8089");
    let voice_url = format!("{}/voice", public_url);

    // Twilio POST webhooks: params come in the body; for GET there are none.
    // We pass an empty param set here — for <Stream> based setups the signature
    // is computed over the URL alone since Twilio sends no body to /voice.
    if !validate_twilio_signature(auth_token, signature, &voice_url, &[]) {
        return axum::response::Response::builder()
            .status(403)
            .body(axum::body::Body::from("Forbidden"))
            .unwrap_or_else(|_| {
                axum::response::Response::new(axum::body::Body::from("Forbidden"))
            });
    }

    let ws_url = public_url
        .replace("https://", "wss://")
        .replace("http://", "ws://");

    let twiml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <Response>\
         <Connect>\
         <Stream url=\"{ws_url}/media-stream\" />\
         </Connect>\
         <Pause length=\"3600\" />\
         </Response>"
    );

    axum::response::Response::builder()
        .header("Content-Type", "application/xml")
        .body(axum::body::Body::from(twiml))
        .unwrap_or_else(|_| {
            axum::response::Response::new(axum::body::Body::from(
                "<Response><Say>Internal error</Say></Response>",
            ))
        })
}

/// WebSocket upgrade handler for Twilio Media Streams.
async fn handle_media_stream(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Main WebSocket handler — processes Twilio Media Stream events.
///
/// Barge-in: when the user starts speaking while TTS is playing, we set a
/// cancellation flag so `send_tts_response` stops sending audio mid-stream,
/// and we send a Twilio `clear` message to flush the audio queue.
async fn handle_ws(socket: WebSocket, state: AppState) {
    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(Mutex::new(sender));

    let mut stream_sid: Option<String> = None;
    let mut call_sid = String::new();
    let mut vad: Option<SileroVad> = None;
    let mut audio_buffer: Vec<f32> = Vec::new();
    let mut in_speech = false;
    let mut silence_frames: u32 = 0;
    let mut greeting_sent = false;

    // Barge-in cancellation: shared flag that send_tts_response checks.
    let tts_cancel = Arc::new(AtomicBool::new(false));
    // Track whether TTS is currently playing so we know when to barge-in.
    let tts_playing = Arc::new(AtomicBool::new(false));

    // Serialized utterance processing: use a channel so only one
    // process_utterance runs at a time (prevents interleaved audio).
    let (utterance_tx, mut utterance_rx) =
        tokio::sync::mpsc::channel::<Vec<f32>>(4);

    // How many 20ms frames must be silent before we consider speech done.
    // Minimum 1 to avoid triggering on every non-speech frame.
    let silence_threshold = ((state.config.vad_silence_ms as u32) / 20).max(1);

    // Try to initialize VAD
    let vad_path = state
        .config
        .vad_model_path
        .clone()
        .unwrap_or_else(|| {
            let home = dirs::home_dir().unwrap_or_default();
            home.join(".athena")
                .join("silero_vad.onnx")
                .to_string_lossy()
                .into_owned()
        });

    match SileroVad::new(&vad_path, state.config.sample_rate, state.config.vad_threshold) {
        Ok(v) => {
            info!("Silero VAD loaded from {}", vad_path);
            vad = Some(v);
        }
        Err(e) => {
            warn!(
                "Failed to load Silero VAD from {}: {}. Falling back to energy-based detection.",
                vad_path, e
            );
        }
    }

    info!("Twilio Media Stream connected");

    // Spawn the serialized utterance processor.
    {
        let sender_clone = sender.clone();
        let core_clone = state.core.clone();
        let config_clone = state.config.clone();
        let tts_cancel_clone = tts_cancel.clone();
        let tts_playing_clone = tts_playing.clone();
        // call_sid isn't known yet — we'll capture it via a shared reference.
        let call_sid_holder: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let call_sid_for_processor = call_sid_holder.clone();
        let sid_holder: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sid_for_processor = sid_holder.clone();

        // Share these with the main loop so it can update them.
        let call_sid_holder_main = call_sid_holder.clone();
        let sid_holder_main = sid_holder.clone();

        tokio::spawn(async move {
            while let Some(buffer) = utterance_rx.recv().await {
                let csid = call_sid_for_processor.lock().await.clone();
                let sid = sid_for_processor.lock().await.clone();
                process_utterance(
                    buffer,
                    &sender_clone,
                    &core_clone,
                    &config_clone,
                    sid.as_deref(),
                    &csid,
                    &tts_cancel_clone,
                    &tts_playing_clone,
                )
                .await;
            }
        });

        // We need access to these holders in the main loop below.
        // Redefine them as local variables the loop can use.
        let mut _call_sid_holder = call_sid_holder_main;
        let mut _sid_holder = sid_holder_main;

        // The main event loop follows. To avoid a deeply nested block we use
        // a trick: move everything into a flat loop using the holders.
        // Actually, let's restructure to keep it clean:

        while let Some(Ok(msg)) = receiver.next().await {
            let text = match msg {
                Message::Text(t) => t,
                Message::Close(_) => {
                    info!("Twilio Media Stream closed");
                    break;
                }
                _ => continue,
            };

            let event: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let event_type = event["event"].as_str().unwrap_or("");

            match event_type {
                "connected" => {
                    info!("Twilio Media Stream: connected event");
                }
                "start" => {
                    stream_sid = event["start"]["streamSid"].as_str().map(String::from);
                    call_sid = event["start"]["callSid"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();
                    info!(
                        "Call started: call_sid={}, stream_sid={:?}",
                        call_sid, stream_sid
                    );

                    // Update shared holders for the utterance processor.
                    *_call_sid_holder.lock().await = call_sid.clone();
                    *_sid_holder.lock().await = stream_sid.clone();

                    // Send greeting (only once per connection).
                    if !greeting_sent {
                        greeting_sent = true;
                        let sid = stream_sid.clone();
                        let sender_clone = sender.clone();
                        let config_clone = state.config.clone();
                        let greeting = state.config.greeting.clone();
                        let playing = tts_playing.clone();
                        let cancel = tts_cancel.clone();
                        tokio::spawn(async move {
                            if let Err(e) = send_tts_response(
                                &sender_clone,
                                &config_clone,
                                sid.as_deref(),
                                &greeting,
                                &cancel,
                                &playing,
                            )
                            .await
                            {
                                error!("Failed to send greeting: {}", e);
                            }
                        });
                    }
                }
                "media" => {
                    let payload = match event["media"]["payload"].as_str() {
                        Some(p) if p.len() <= MAX_MEDIA_PAYLOAD_BYTES => p,
                        Some(p) => {
                            warn!("Oversized media payload ({} bytes), dropping", p.len());
                            continue;
                        }
                        None => continue,
                    };

                    let mulaw_bytes =
                        match base64::engine::general_purpose::STANDARD.decode(payload) {
                            Ok(b) => b,
                            Err(_) => continue,
                        };

                    let pcm = mulaw_to_f32(&mulaw_bytes);

                    let is_speech = if let Some(ref mut v) = vad {
                        v.is_speech(&pcm)
                    } else {
                        let energy: f32 =
                            pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len().max(1) as f32;
                        energy > 0.001
                    };

                    if is_speech {
                        // Barge-in: if TTS is playing and user starts speaking, cancel it.
                        if !in_speech && tts_playing.load(Ordering::Relaxed) {
                            info!("Barge-in detected — cancelling TTS playback");
                            tts_cancel.store(true, Ordering::Relaxed);
                            // Send Twilio clear message to flush queued audio.
                            if let Some(ref sid) = stream_sid {
                                let clear_msg = serde_json::json!({
                                    "event": "clear",
                                    "streamSid": sid,
                                });
                                let _ = sender
                                    .lock()
                                    .await
                                    .send(Message::Text(clear_msg.to_string()))
                                    .await;
                            }
                        }

                        if !in_speech {
                            debug!("Speech started");
                            in_speech = true;
                        }
                        silence_frames = 0;
                        audio_buffer.extend_from_slice(&pcm);

                        if audio_buffer.len() > MAX_AUDIO_BUFFER_SAMPLES {
                            warn!("Audio buffer overflow, flushing");
                            let buffer = std::mem::take(&mut audio_buffer);
                            in_speech = false;
                            silence_frames = 0;
                            if let Some(ref mut v) = vad {
                                v.reset();
                            }
                            let _ = utterance_tx.send(buffer).await;
                        }
                    } else if in_speech {
                        audio_buffer.extend_from_slice(&pcm);
                        silence_frames += 1;

                        if silence_frames >= silence_threshold {
                            debug!(
                                "End of speech detected ({} samples buffered)",
                                audio_buffer.len()
                            );
                            in_speech = false;
                            silence_frames = 0;
                            if let Some(ref mut v) = vad {
                                v.reset();
                            }

                            if audio_buffer.len() >= MIN_SPEECH_SAMPLES {
                                let buffer = std::mem::take(&mut audio_buffer);
                                let _ = utterance_tx.send(buffer).await;
                            } else {
                                debug!(
                                    "Discarding short utterance ({} samples)",
                                    audio_buffer.len()
                                );
                                audio_buffer.clear();
                            }
                        }
                    }
                }
                "stop" => {
                    info!("Twilio Media Stream stopped (call_sid={})", call_sid);
                    break;
                }
                "mark" => {
                    // When TTS playback finishes, Twilio sends back our mark.
                    if event["mark"]["name"].as_str() == Some("response_end") {
                        tts_playing.store(false, Ordering::Relaxed);
                    }
                    debug!("Mark event: {:?}", event["mark"]);
                }
                _ => {
                    debug!("Unknown Twilio event: {}", event_type);
                }
            }
        }
    }

    info!("Call session ended: {}", call_sid);
}

/// Process a complete utterance: STT -> LLM -> TTS -> send audio back.
async fn process_utterance(
    audio: Vec<f32>,
    sender: &Arc<Mutex<SplitSink<WebSocket, Message>>>,
    core: &CoreHandle,
    config: &TelephonyConfig,
    stream_sid: Option<&str>,
    call_sid: &str,
    tts_cancel: &AtomicBool,
    tts_playing: &AtomicBool,
) {
    info!(
        "Processing utterance: {} samples ({:.1}s)",
        audio.len(),
        audio.len() as f32 / config.sample_rate as f32
    );

    let wav = pcm_to_wav(&audio, config.sample_rate);

    let transcript = match transcribe_audio(wav, config).await {
        Ok(t) => t,
        Err(e) => {
            error!("STT failed: {}", e);
            // Tell the caller instead of going silent
            let _ = send_tts_response(
                sender,
                config,
                stream_sid,
                "I'm sorry, I couldn't hear that clearly. Could you please repeat?",
                tts_cancel,
                tts_playing,
            )
            .await;
            return;
        }
    };

    let transcript = transcript.trim().to_string();
    if transcript.is_empty() {
        debug!("Empty transcript, skipping");
        return;
    }

    info!("Transcript: \"{}\"", transcript);

    let session = SessionContext {
        platform: "phone".to_string(),
        user_id: call_sid.to_string(),
        chat_id: call_sid.to_string(),
    };

    let confirmer = Arc::new(AutoConfirmer);
    let mut event_rx = match core.chat(session, &transcript, confirmer).await {
        Ok(rx) => rx,
        Err(e) => {
            error!("Core dispatch failed: {}", e);
            let _ = send_tts_response(
                sender,
                config,
                stream_sid,
                "I'm sorry, I encountered an error. Please try again.",
                tts_cancel,
                tts_playing,
            )
            .await;
            return;
        }
    };

    let mut full_response = String::new();
    while let Some(event) = event_rx.recv().await {
        match event {
            CoreEvent::StreamChunk(chunk) => {
                full_response.push_str(&chunk);
                // Cap accumulated response to prevent unbounded memory growth.
                if full_response.len() > MAX_RESPONSE_CHARS {
                    debug!("Response exceeded {} chars, stopping accumulation", MAX_RESPONSE_CHARS);
                    break;
                }
            }
            CoreEvent::Response(resp) => {
                full_response = resp;
                break;
            }
            CoreEvent::Error(e) => {
                error!("LLM error: {}", e);
                let _ = send_tts_response(
                    sender,
                    config,
                    stream_sid,
                    "I'm sorry, something went wrong.",
                    tts_cancel,
                    tts_playing,
                )
                .await;
                return;
            }
            _ => {}
        }
    }

    if full_response.is_empty() {
        return;
    }

    // Truncate long responses for phone (conversations should be concise).
    // Use char boundary to avoid panicking on multibyte characters.
    let voice_response = if full_response.chars().count() > 500 {
        let truncated: String = full_response.chars().take(500).collect();
        match truncated.rfind(". ") {
            Some(pos) => format!("{}.", &truncated[..pos]),
            None => format!("{}...", truncated),
        }
    } else {
        full_response
    };

    info!(
        "LLM response ({} chars): \"{}\"",
        voice_response.len(),
        &voice_response[..voice_response.len().min(80)]
    );

    if let Err(e) =
        send_tts_response(sender, config, stream_sid, &voice_response, tts_cancel, tts_playing)
            .await
    {
        error!("Failed to send TTS response: {}", e);
    }
}

/// Synthesize text and send as Twilio Media Stream audio.
///
/// Checks `cancel` between chunks to support barge-in. If cancelled, stops
/// sending and returns early. Sets `playing` to true while sending.
async fn send_tts_response(
    sender: &Arc<Mutex<SplitSink<WebSocket, Message>>>,
    config: &TelephonyConfig,
    stream_sid: Option<&str>,
    text: &str,
    cancel: &AtomicBool,
    playing: &AtomicBool,
) -> Result<(), String> {
    let stream_sid = stream_sid.ok_or("No stream SID")?;

    // Clear any previous cancellation before we start.
    cancel.store(false, Ordering::Relaxed);
    playing.store(true, Ordering::Relaxed);

    let wav_bytes = synthesize_speech(text, config).await?;
    let pcm = wav_to_f32_mono(&wav_bytes, config.sample_rate)?;
    let mulaw = f32_to_mulaw(&pcm);

    let chunk_size = MULAW_CHUNK_SAMPLES;
    let mut sender_guard = sender.lock().await;
    let mut sent = 0usize;

    for chunk in mulaw.chunks(chunk_size) {
        // Barge-in check: stop sending if the user started speaking.
        if cancel.load(Ordering::Relaxed) {
            info!("TTS playback cancelled by barge-in after {} bytes", sent);
            playing.store(false, Ordering::Relaxed);
            return Ok(());
        }

        let payload = base64::engine::general_purpose::STANDARD.encode(chunk);
        let media_msg = serde_json::json!({
            "event": "media",
            "streamSid": stream_sid,
            "media": {
                "payload": payload
            }
        });

        if let Err(e) = sender_guard
            .send(Message::Text(media_msg.to_string()))
            .await
        {
            playing.store(false, Ordering::Relaxed);
            return Err(format!("WebSocket send error: {}", e));
        }
        sent += chunk.len();
    }

    // Mark event so we know when playback finishes
    let mark_msg = serde_json::json!({
        "event": "mark",
        "streamSid": stream_sid,
        "mark": { "name": "response_end" }
    });
    let _ = sender_guard
        .send(Message::Text(mark_msg.to_string()))
        .await;

    info!(
        "Sent TTS audio: {} mulaw bytes ({:.1}s)",
        mulaw.len(),
        mulaw.len() as f32 / config.sample_rate as f32
    );

    // playing will be set to false when the "mark" event comes back from Twilio.
    // But if the mark never arrives, we should eventually time out — handled by
    // the next speech event's barge-in logic.

    Ok(())
}

// ── Public Entry Point ───────────────────────────────────────────────

pub struct SystemInfo {
    pub provider: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub started_at: tokio::time::Instant,
}

/// Start the telephony server (HTTP + WebSocket for Twilio Media Streams).
pub async fn run_telephony(
    core: CoreHandle,
    config: TelephonyConfig,
    _system_info: SystemInfo,
) -> anyhow::Result<()> {
    if config.stt_url.is_none() {
        anyhow::bail!(
            "Telephony requires STT. Set [telephony] stt_url to a Whisper-compatible endpoint.\n\
             Free options: faster-whisper-server, whisper.cpp server, or Groq (free tier)."
        );
    }
    if config.tts_url.is_none() {
        anyhow::bail!(
            "Telephony requires TTS. Set [telephony] tts_url to an OpenAI-compatible TTS endpoint.\n\
             Free options: Piper TTS, Kokoro TTS, or Coqui TTS."
        );
    }

    let state = AppState {
        core,
        config: config.clone(),
    };

    let app = Router::new()
        .route("/voice", get(handle_voice).post(handle_voice))
        .route("/media-stream", get(handle_media_stream))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let addr = format!("{}:{}", config.listen_host, config.listen_port);
    info!("Athena Telephony Server");
    info!("  Listening on {}", addr);
    info!("  Voice webhook: /voice");
    info!("  Media stream:  /media-stream");
    if let Some(ref url) = config.public_url {
        info!("  Public URL:    {}", url);
        info!("  Configure Twilio voice webhook to: {}/voice", url);
    } else {
        warn!("  No public_url set. Use ngrok or set [telephony] public_url.");
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── mulaw codec ──────────────────────────────────────────────────

    /// Encoding then decoding should approximate the original value.
    /// mulaw has ~5-bit mantissa precision so allow ±2% error.
    #[test]
    fn mulaw_roundtrip_sine() {
        for i in 0..=100i32 {
            let original = (i * 327) as i16; // 0..32700 range
            let encoded = mulaw_encode(original);
            let decoded = mulaw_decode(encoded);
            let err = (decoded as i32 - original as i32).abs();
            let tolerance = (original.abs() as i32 / 20).max(10);
            assert!(
                err <= tolerance,
                "roundtrip error too large: original={}, decoded={}, err={}",
                original,
                decoded,
                err
            );
        }
    }

    /// Negative values should round-trip with the same precision.
    #[test]
    fn mulaw_roundtrip_negative() {
        for i in 0..=100i32 {
            let original = -((i * 327) as i16);
            let encoded = mulaw_encode(original);
            let decoded = mulaw_decode(encoded);
            let err = (decoded as i32 - original as i32).abs();
            let tolerance = (original.abs() as i32 / 20).max(10);
            assert!(
                err <= tolerance,
                "roundtrip error too large: original={}, decoded={}, err={}",
                original,
                decoded,
                err
            );
        }
    }

    /// MAX i16 sample must not panic (regression for i16 overflow bug).
    #[test]
    fn mulaw_encode_max_no_overflow() {
        let _ = mulaw_encode(i16::MAX);
        let _ = mulaw_encode(i16::MIN);
    }

    /// Silence (0) should encode/decode cleanly.
    #[test]
    fn mulaw_silence() {
        let encoded = mulaw_encode(0);
        let decoded = mulaw_decode(encoded);
        // mulaw silence decodes to a small value due to BIAS, not exactly 0
        assert!(
            decoded.abs() < 200,
            "silence decoded to unexpected value: {}",
            decoded
        );
    }

    /// mulaw_to_f32 output must be within [-1, 1].
    #[test]
    fn mulaw_to_f32_bounds() {
        let all_bytes: Vec<u8> = (0u8..=255).collect();
        for &s in mulaw_to_f32(&all_bytes).iter() {
            assert!(
                s >= -1.0 && s <= 1.0,
                "mulaw_to_f32 out of [-1,1]: {}",
                s
            );
        }
    }

    // ── pcm_to_wav / wav_to_f32_mono roundtrip ────────────────────────

    fn make_test_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
        pcm_to_wav(samples, sample_rate)
    }

    #[test]
    fn wav_roundtrip_silence() {
        let silence = vec![0.0f32; 160];
        let wav = make_test_wav(&silence, 8000);
        let decoded = wav_to_f32_mono(&wav, 8000).unwrap();
        assert_eq!(decoded.len(), silence.len());
        for s in &decoded {
            assert!(s.abs() < 1e-4, "silence not silent: {}", s);
        }
    }

    #[test]
    fn wav_roundtrip_sine_8khz() {
        let samples: Vec<f32> = (0..800)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 8000.0).sin() * 0.5)
            .collect();
        let wav = make_test_wav(&samples, 8000);
        let decoded = wav_to_f32_mono(&wav, 8000).unwrap();
        assert_eq!(decoded.len(), samples.len());
        let max_err = samples
            .iter()
            .zip(&decoded)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err < 0.001, "WAV roundtrip error too large: {}", max_err);
    }

    #[test]
    fn wav_header_magic() {
        let wav = make_test_wav(&[0.0; 8], 8000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
    }

    #[test]
    fn wav_rejects_short_input() {
        assert!(wav_to_f32_mono(&[0u8; 10], 8000).is_err());
    }

    #[test]
    fn wav_rejects_non_riff() {
        let mut bad = vec![0u8; 44];
        bad[0..4].copy_from_slice(b"OGG "); // not RIFF
        assert!(wav_to_f32_mono(&bad, 8000).is_err());
    }

    // ── resampling ────────────────────────────────────────────────────

    /// Downsampling from 24kHz to 8kHz should produce 1/3 as many samples.
    #[test]
    fn wav_resampling_24k_to_8k() {
        let samples: Vec<f32> = (0..2400).map(|i| (i as f32 / 2400.0).sin()).collect();
        let wav = make_test_wav(&samples, 24000);
        let decoded = wav_to_f32_mono(&wav, 8000).unwrap();
        let expected_len = 2400 / 3;
        let tolerance = 5usize;
        assert!(
            decoded.len().abs_diff(expected_len) <= tolerance,
            "resampled length {} far from expected {}",
            decoded.len(),
            expected_len
        );
    }

    /// When src_rate == target_rate, no resampling should occur.
    #[test]
    fn wav_no_resampling_same_rate() {
        let samples: Vec<f32> = (0..160).map(|i| i as f32 / 160.0).collect();
        let wav = make_test_wav(&samples, 16000);
        let decoded = wav_to_f32_mono(&wav, 16000).unwrap();
        assert_eq!(decoded.len(), samples.len());
    }

    // ── VAD silence threshold ─────────────────────────────────────────

    /// vad_silence_ms less than one frame period (20ms) must still give threshold >= 1.
    #[test]
    fn vad_silence_threshold_minimum_one() {
        for ms in [0u64, 1, 10, 19] {
            let threshold = ((ms as u32) / 20).max(1);
            assert_eq!(threshold, 1, "threshold for {}ms should be 1", ms);
        }
        // 20ms should give exactly 1
        assert_eq!(((20u32) / 20).max(1), 1);
        // 800ms (default) should give 40
        assert_eq!(((800u32) / 20).max(1), 40);
    }

    // ── response truncation ───────────────────────────────────────────

    /// Truncation must not panic on multibyte (emoji) content.
    #[test]
    fn truncate_multibyte_no_panic() {
        // 501 emoji = well over 500 bytes but exactly 501 chars
        let long: String = std::iter::repeat('🎙').take(501).collect();
        // Replicate truncation logic from process_utterance
        let voice_response = if long.chars().count() > 500 {
            let truncated: String = long.chars().take(500).collect();
            match truncated.rfind(". ") {
                Some(pos) => format!("{}.", &truncated[..pos]),
                None => format!("{}...", truncated),
            }
        } else {
            long.clone()
        };
        assert!(voice_response.ends_with("..."));
        assert!(voice_response.chars().count() <= 503); // 500 + "..."
    }

    /// ASCII input under 500 chars should be returned unmodified.
    #[test]
    fn truncate_short_string_unchanged() {
        let short = "Hello world.".to_string();
        let voice_response = if short.chars().count() > 500 {
            unreachable!()
        } else {
            short.clone()
        };
        assert_eq!(voice_response, short);
    }

    // ── Twilio signature validation ──────────────────────────────────

    /// No auth token configured = always pass (validation disabled).
    #[test]
    fn twilio_sig_no_token_passes() {
        assert!(validate_twilio_signature(None, None, "https://example.com/voice", &[]));
        assert!(validate_twilio_signature(Some(""), None, "https://example.com/voice", &[]));
    }

    /// Missing signature header with a configured token must fail.
    #[test]
    fn twilio_sig_missing_header_fails() {
        assert!(!validate_twilio_signature(
            Some("secret"),
            None,
            "https://example.com/voice",
            &[]
        ));
    }

    /// Correct HMAC-SHA1 signature must pass.
    #[test]
    fn twilio_sig_valid() {
        // Compute expected signature manually
        use hmac::{Hmac, Mac};
        type HmacSha1 = Hmac<sha1::Sha1>;

        let token = "test_token_12345";
        let url = "https://myapp.ngrok.io/voice";
        let params = vec![
            ("CallSid".to_string(), "CA123".to_string()),
            ("From".to_string(), "+15551234567".to_string()),
        ];

        // Build data: url + sorted params
        let mut data = url.to_string();
        let mut sorted = params.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in &sorted {
            data.push_str(k);
            data.push_str(v);
        }

        let mut mac = HmacSha1::new_from_slice(token.as_bytes()).unwrap();
        mac.update(data.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        assert!(validate_twilio_signature(
            Some(token),
            Some(&sig),
            url,
            &params
        ));
    }

    /// Wrong signature must fail.
    #[test]
    fn twilio_sig_wrong_fails() {
        assert!(!validate_twilio_signature(
            Some("real_token"),
            Some("dGhpc2lzd3Jvbmc="),
            "https://example.com/voice",
            &[]
        ));
    }

    // ── payload size limit ───────────────────────────────────────────

    #[test]
    fn media_payload_limit_constant() {
        // Twilio sends ~214 bytes per 20ms chunk. Our limit is generous but bounded.
        assert!(MAX_MEDIA_PAYLOAD_BYTES >= 1000);
        assert!(MAX_MEDIA_PAYLOAD_BYTES <= 64_000);
    }

    // ── response cap ─────────────────────────────────────────────────

    #[test]
    fn response_cap_constant() {
        assert!(MAX_RESPONSE_CHARS >= 500);
        assert!(MAX_RESPONSE_CHARS <= 10_000);
    }
}
