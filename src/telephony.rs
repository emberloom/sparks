//! Telephony module — Twilio Media Streams integration for phone calls.
//!
//! Architecture:
//!   Caller → Twilio → HTTP /voice (TwiML) → Twilio connects WebSocket → /media-stream
//!   Audio chunks (mulaw 8kHz) → Silero VAD → end-of-speech → STT (Whisper) → LLM → TTS → audio back
//!
//! STT: any Whisper-compatible endpoint (faster-whisper, whisper.cpp, etc.) — free & open-source.
//! TTS: any OpenAI-compatible TTS endpoint (Piper, Coqui, Kokoro, etc.) — configurable.

use std::sync::Arc;
use tokio::sync::Mutex;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
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

/// Encode 16-bit PCM sample to mulaw byte.
fn mulaw_encode(sample: i16) -> u8 {
    const BIAS: i16 = 0x84;
    const MAX: i16 = 0x7FFF;

    let sign: u8;
    let mut sample = sample;
    if sample < 0 {
        sign = 0x80;
        sample = -sample;
    } else {
        sign = 0;
    }
    if sample > MAX {
        sample = MAX;
    }
    sample += BIAS;

    let mut exponent: u8 = 7;
    let mut mask: i16 = 0x4000;
    while exponent > 0 {
        if (sample & mask) != 0 {
            break;
        }
        exponent -= 1;
        mask >>= 1;
    }

    let mantissa = ((sample >> (exponent as i16 + 3)) & 0x0F) as u8;
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
fn wav_to_f32_mono(wav_bytes: &[u8], target_rate: u32) -> Result<Vec<f32>, String> {
    if wav_bytes.len() < 44 {
        return Err("WAV too short".into());
    }
    let channels = u16::from_le_bytes([wav_bytes[22], wav_bytes[23]]) as usize;
    let src_rate = u32::from_le_bytes([
        wav_bytes[24],
        wav_bytes[25],
        wav_bytes[26],
        wav_bytes[27],
    ]);
    let bits = u16::from_le_bytes([wav_bytes[34], wav_bytes[35]]);

    // Find data chunk
    let mut pos = 12;
    while pos + 8 < wav_bytes.len() {
        let chunk_id = &wav_bytes[pos..pos + 4];
        let chunk_size = u32::from_le_bytes([
            wav_bytes[pos + 4],
            wav_bytes[pos + 5],
            wav_bytes[pos + 6],
            wav_bytes[pos + 7],
        ]) as usize;
        if chunk_id == b"data" {
            let end = std::cmp::min(pos + 8 + chunk_size, wav_bytes.len());
            let data = &wav_bytes[pos + 8..end];
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
            return Ok(samples);
        }
        pos += 8 + chunk_size;
        if chunk_size % 2 != 0 {
            pos += 1;
        }
    }
    Err("WAV data chunk not found".into())
}

// ── Shared App State ─────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    core: CoreHandle,
    config: TelephonyConfig,
}

// ── HTTP Handlers ────────────────────────────────────────────────────

/// TwiML response — tells Twilio to connect a Media Stream WebSocket.
async fn handle_voice(State(state): State<AppState>) -> impl IntoResponse {
    let public_url = state
        .config
        .public_url
        .as_deref()
        .unwrap_or("http://localhost:8089");

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
        .unwrap()
}

/// WebSocket upgrade handler for Twilio Media Streams.
async fn handle_media_stream(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Main WebSocket handler — processes Twilio Media Stream events.
async fn handle_ws(socket: WebSocket, state: AppState) {
    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(Mutex::new(sender));

    let mut stream_sid: Option<String> = None;
    let mut call_sid = String::new();
    let mut vad: Option<SileroVad> = None;
    let mut audio_buffer: Vec<f32> = Vec::new();
    let mut in_speech = false;
    let mut silence_frames: u32 = 0;

    let silence_threshold = (state.config.vad_silence_ms as u32) / 20;

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

    let greeting_sender = sender.clone();
    let greeting_config = state.config.clone();
    let greeting_text = state.config.greeting.clone();

    info!("Twilio Media Stream connected");

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

                let sid = stream_sid.clone();
                let sender_clone = greeting_sender.clone();
                let config_clone = greeting_config.clone();
                let greeting = greeting_text.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        send_tts_response(&sender_clone, &config_clone, sid.as_deref(), &greeting)
                            .await
                    {
                        error!("Failed to send greeting: {}", e);
                    }
                });
            }
            "media" => {
                let payload = match event["media"]["payload"].as_str() {
                    Some(p) => p,
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
                        let sender_clone = sender.clone();
                        let core_clone = state.core.clone();
                        let config_clone = state.config.clone();
                        let sid = stream_sid.clone();
                        let csid = call_sid.clone();
                        tokio::spawn(async move {
                            process_utterance(
                                buffer,
                                &sender_clone,
                                &core_clone,
                                &config_clone,
                                sid.as_deref(),
                                &csid,
                            )
                            .await;
                        });
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
                            let sender_clone = sender.clone();
                            let core_clone = state.core.clone();
                            let config_clone = state.config.clone();
                            let sid = stream_sid.clone();
                            let csid = call_sid.clone();
                            tokio::spawn(async move {
                                process_utterance(
                                    buffer,
                                    &sender_clone,
                                    &core_clone,
                                    &config_clone,
                                    sid.as_deref(),
                                    &csid,
                                )
                                .await;
                            });
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
                debug!("Mark event: {:?}", event["mark"]);
            }
            _ => {
                debug!("Unknown Twilio event: {}", event_type);
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

    // Truncate long responses for phone (conversations should be concise)
    let voice_response = if full_response.len() > 500 {
        let truncated = &full_response[..500];
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

    if let Err(e) = send_tts_response(sender, config, stream_sid, &voice_response).await {
        error!("Failed to send TTS response: {}", e);
    }
}

/// Synthesize text and send as Twilio Media Stream audio.
async fn send_tts_response(
    sender: &Arc<Mutex<SplitSink<WebSocket, Message>>>,
    config: &TelephonyConfig,
    stream_sid: Option<&str>,
    text: &str,
) -> Result<(), String> {
    let stream_sid = stream_sid.ok_or("No stream SID")?;

    let wav_bytes = synthesize_speech(text, config).await?;
    let pcm = wav_to_f32_mono(&wav_bytes, config.sample_rate)?;
    let mulaw = f32_to_mulaw(&pcm);

    let chunk_size = MULAW_CHUNK_SAMPLES;
    let mut sender_guard = sender.lock().await;

    for chunk in mulaw.chunks(chunk_size) {
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
            return Err(format!("WebSocket send error: {}", e));
        }
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
