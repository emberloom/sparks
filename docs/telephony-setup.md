# Telephony Setup Guide

Phone call support for Athena via Twilio Media Streams.

**Architecture**: Caller → Twilio → `POST /voice` (TwiML) → WebSocket `/media-stream` → mulaw 8kHz → Silero VAD → STT (Whisper) → LLM → TTS → audio back to caller.

```
┌──────────┐     ┌──────────┐     ┌──────────────────────────────────────┐
│  Caller  │────▶│  Twilio  │────▶│  Athena Telephony Server             │
│  (phone) │◀────│  (PSTN)  │◀────│                                      │
└──────────┘     └──────────┘     │  /voice ─▶ TwiML (connect stream)   │
                                  │  /media-stream ─▶ WebSocket          │
                                  │    ├─ Silero VAD (speech detection)  │
                                  │    ├─ Whisper STT (transcription)    │
                                  │    ├─ LLM (response generation)     │
                                  │    └─ TTS (speech synthesis)        │
                                  └──────────────────────────────────────┘
```

---

## Prerequisites

| Component | Purpose | Options |
|-----------|---------|---------|
| Twilio account | Phone number + Media Streams | [twilio.com/try-twilio](https://www.twilio.com/try-twilio) (free trial) |
| STT server | Speech-to-Text | faster-whisper-server, whisper.cpp, Groq (free tier) |
| TTS server | Text-to-Speech | Piper TTS, Kokoro TTS, OpenAI TTS |
| Silero VAD model | Voice Activity Detection | ~2MB ONNX file, runs locally |
| Public URL | Twilio webhook reachability | ngrok, Cloudflare Tunnel, or public server |

---

## Step 1: Download the Silero VAD Model

```bash
mkdir -p ~/.athena
curl -L -o ~/.athena/silero_vad.onnx \
  https://github.com/snakers4/silero-vad/raw/master/src/silero_vad/data/silero_vad.onnx
```

Verify: `ls -lh ~/.athena/silero_vad.onnx` — should be ~2MB.

## Step 2: Start an STT Server

Pick one:

**Option A — faster-whisper-server (recommended, self-hosted, free)**
```bash
pip install faster-whisper-server
faster-whisper-server --model Systran/faster-whisper-large-v3 --port 8787
```

**Option B — whisper.cpp (self-hosted, free)**
```bash
# Build whisper.cpp, then:
./server -m models/ggml-large-v3.bin --port 8787
```

**Option C — Groq (cloud, free tier)**
No server needed — just set the URL and API key in config:
```toml
stt_url = "https://api.groq.com/openai/v1/audio/transcriptions"
stt_api_key = "gsk_..."   # or ATHENA_STT_API_KEY env var
```

## Step 3: Start a TTS Server

Pick one:

**Option A — Piper TTS (self-hosted, free)**
```bash
pip install piper-tts
# See piper-tts docs for server mode
```

**Option B — Kokoro TTS (self-hosted, free)**
```bash
pip install kokoro
# Start with OpenAI-compatible endpoint on port 8880
```

**Option C — OpenAI TTS (cloud, $0.015/1K chars)**
```toml
tts_url = "https://api.openai.com/v1/audio/speech"
tts_api_key = "sk-..."   # or ATHENA_TTS_API_KEY env var
```

## Step 4: Expose Athena with a Public URL

Twilio needs to reach your server. Use ngrok for local development:

```bash
ngrok http 8089
# Note the https://xxxx.ngrok.io URL
```

## Step 5: Configure Athena

Add to your `config.toml`:

```toml
[telephony]
twilio_account_sid = "ACxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
twilio_auth_token  = "your_auth_token_here"
listen_host = "0.0.0.0"
listen_port = 8089
public_url = "https://xxxx.ngrok.io"     # from Step 4

# STT (adjust to match your choice from Step 2)
stt_url   = "http://localhost:8787/v1/audio/transcriptions"
stt_model = "whisper-large-v3"

# TTS (adjust to match your choice from Step 3)
tts_url   = "http://localhost:8880/v1/audio/speech"
tts_model = "tts-1"
tts_voice = "alloy"

# Optional tuning
greeting       = "Hello, this is Athena. How can I help you?"
vad_silence_ms = 800    # ms of silence before end-of-speech (lower = faster, more false positives)
vad_threshold  = 0.5    # speech probability threshold (higher = stricter)
```

Credentials can also be set via environment variables:
```bash
export ATHENA_TWILIO_ACCOUNT_SID="AC..."
export ATHENA_TWILIO_AUTH_TOKEN="..."
export ATHENA_STT_API_KEY="..."        # only if using cloud STT
export ATHENA_TTS_API_KEY="..."        # only if using cloud TTS
```

## Step 6: Configure Twilio

1. Go to **Twilio Console** → **Phone Numbers** → **Manage** → select your number
2. Under **Voice Configuration**:
   - Set **"A call comes in"** webhook to: `https://xxxx.ngrok.io/voice`
   - Method: **HTTP POST**
3. Save

## Step 7: Build and Run

```bash
cargo run --features telephony -- telephony
```

You should see:
```
Athena Telephony Server
  Listening on 0.0.0.0:8089
  Voice webhook: /voice
  Media stream:  /media-stream
  Public URL:    https://xxxx.ngrok.io
  Configure Twilio voice webhook to: https://xxxx.ngrok.io/voice
```

## Step 8: Test

Call your Twilio phone number. You should hear the greeting, then be able to have a conversation.

---

## Security

### Webhook Authentication

When `twilio_auth_token` is configured (recommended), Athena validates the `X-Twilio-Signature` HMAC-SHA1 header on every `/voice` request. Unauthenticated requests receive a `403 Forbidden` response.

If the token is omitted, signature validation is skipped (useful for local development).

### Payload Limits

- Media payloads exceeding 32KB are dropped (Twilio normally sends ~214 bytes per 20ms chunk)
- LLM response accumulation is capped at 2,000 characters
- Audio buffer is capped at 60 seconds (480,000 samples)

---

## Barge-in (Interrupt)

Users can interrupt Athena mid-response by speaking. When speech is detected during TTS playback:

1. TTS chunk sending stops immediately
2. A Twilio `clear` event flushes queued audio
3. The user's new utterance is captured and processed normally

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| No answer / timeout | Twilio can't reach `/voice` | Check ngrok is running, URL matches Twilio config |
| `403 Forbidden` in Twilio logs | Signature mismatch | Verify `twilio_auth_token` matches Twilio Console, check `public_url` matches exactly |
| Greeting plays but no response | STT server not running or unreachable | Check `stt_url` is accessible: `curl http://localhost:8787/health` |
| Garbled / silent response | TTS server not running | Check `tts_url` is accessible |
| "I couldn't hear that clearly" | STT transcription failed | Check STT server logs, ensure audio is reaching the server |
| Very slow response | VAD model not found, using energy fallback | Download Silero VAD model (Step 1) |
| Cuts off mid-word | `vad_silence_ms` too low | Increase to 800-1000ms |
| Long pauses before response | `vad_silence_ms` too high | Decrease to 500-600ms |

### Logs

Run with debug logging for full audio pipeline visibility:

```bash
RUST_LOG=athena::telephony=debug cargo run --features telephony -- telephony
```

---

## PR Testing Checklist

Quick validation for reviewing telephony PRs:

### Build & Unit Tests
```bash
# Build with telephony feature
cargo build --features telephony

# Run all telephony unit tests (21 tests)
cargo test --features telephony telephony::

# Expected: mulaw codec, WAV roundtrip, Twilio signature validation,
# payload limits, response cap, truncation safety — all pass
```

### Local Smoke Test (no Twilio needed)

```bash
# 1. Start Athena telephony server
cargo run --features telephony -- telephony

# 2. Verify health endpoint
curl http://localhost:8089/health
# Expected: "ok"

# 3. Verify /voice returns TwiML (no auth token = no signature check)
curl -X POST http://localhost:8089/voice
# Expected: XML with <Response><Connect><Stream url="..."/></Connect>...</Response>

# 4. Verify /voice rejects bad signature (when auth token is set)
# Set twilio_auth_token in config, then:
curl -X POST http://localhost:8089/voice
# Expected: "Forbidden" (403) — no X-Twilio-Signature header

# 5. Verify WebSocket endpoint accepts upgrade
# Use websocat or wscat:
wscat -c ws://localhost:8089/media-stream
# Expected: connection opens (send {"event":"connected"} to test)
```

### End-to-End Test (requires Twilio)

1. Configure Twilio + ngrok + STT + TTS (Steps 1-6 above)
2. Call the Twilio number
3. Verify:
   - [ ] Greeting plays on answer
   - [ ] Speech is transcribed (check debug logs for `Transcript: "..."`)
   - [ ] LLM response is spoken back
   - [ ] Barge-in works (speak during response — it should stop and listen)
   - [ ] Long silence after speaking triggers end-of-speech correctly
   - [ ] Call disconnect is handled cleanly (no errors in logs)
   - [ ] Multiple back-and-forth turns work in a single call
