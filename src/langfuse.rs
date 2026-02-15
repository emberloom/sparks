use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

/// Langfuse observability client using the batch ingestion API.
///
/// All trace/span/generation events are fire-and-forget: they are sent
/// via `tokio::spawn` and never block the caller. Errors are logged as
/// warnings but otherwise swallowed.
#[derive(Clone)]
pub struct LangfuseClient {
    client: reqwest::Client,
    public_key: String,
    secret_key: String,
    base_url: String,
}

impl LangfuseClient {
    pub fn new(public_key: String, secret_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            public_key,
            secret_key,
            base_url: base_url.unwrap_or_else(|| "https://cloud.langfuse.com".to_string()),
        }
    }

    /// Fire-and-forget batch ingestion via `POST /api/public/ingestion`.
    /// Uses Basic Auth (public_key:secret_key). Never blocks the caller.
    pub fn ingest(&self, events: Vec<serde_json::Value>) {
        if events.is_empty() {
            return;
        }
        let client = self.client.clone();
        let url = format!("{}/api/public/ingestion", self.base_url);
        let public_key = self.public_key.clone();
        let secret_key = self.secret_key.clone();

        tokio::spawn(async move {
            let body = serde_json::json!({ "batch": events });
            match client
                .post(&url)
                .basic_auth(&public_key, Some(&secret_key))
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if !resp.status().is_success() => {
                    tracing::warn!("Langfuse ingestion failed: {}", resp.status());
                }
                Err(e) => {
                    tracing::warn!("Langfuse ingestion error: {}", e);
                }
                _ => {}
            }
        });
    }

    /// Ingest events synchronously. Useful for short-lived CLI commands.
    pub async fn ingest_sync(&self, events: Vec<serde_json::Value>) -> std::result::Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        let url = format!("{}/api/public/ingestion", self.base_url);
        let body = serde_json::json!({ "batch": events });
        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.public_key, Some(&self.secret_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Langfuse ingestion error: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("Langfuse ingestion failed: {}", resp.status()));
        }
        Ok(())
    }

    /// Emit a mission KPI snapshot as a trace event.
    pub async fn emit_kpi_snapshot(
        &self,
        lane: &str,
        repo: &str,
        risk_tier: &str,
        payload: serde_json::Value,
    ) -> std::result::Result<(), String> {
        let trace_id = new_id();
        let body = serde_json::json!({
            "id": trace_id,
            "name": "mission:kpi_snapshot",
            "timestamp": now_iso(),
            "sessionId": repo,
            "userId": "athena",
            "tags": ["mission", "kpi", lane, risk_tier],
            "input": {
                "lane": lane,
                "repo": repo,
                "risk_tier": risk_tier,
            },
            "output": payload,
        });
        self.ingest_sync(vec![ingestion_event("trace-create", body)]).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Build a batch-ingestion event envelope.
fn ingestion_event(event_type: &str, body: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": new_id(),
        "type": event_type,
        "timestamp": now_iso(),
        "body": body,
    })
}

// ---------------------------------------------------------------------------
// ActiveTrace — a live trace with nested spans and generations
// ---------------------------------------------------------------------------

pub struct ActiveTrace {
    client: Arc<LangfuseClient>,
    trace_id: String,
}

impl ActiveTrace {
    /// Create a new trace. Immediately sends a `trace-create` event.
    pub fn start(
        client: Arc<LangfuseClient>,
        name: &str,
        user_id: Option<&str>,
        session_id: Option<&str>,
        input: Option<&str>,
        tags: Vec<&str>,
    ) -> Self {
        let trace_id = new_id();
        let mut body = serde_json::json!({
            "id": trace_id,
            "name": name,
            "timestamp": now_iso(),
            "tags": tags,
        });
        if let Some(uid) = user_id {
            body["userId"] = serde_json::json!(uid);
        }
        if let Some(sid) = session_id {
            body["sessionId"] = serde_json::json!(sid);
        }
        if let Some(inp) = input {
            body["input"] = serde_json::json!(inp);
        }
        client.ingest(vec![ingestion_event("trace-create", body)]);
        Self { client, trace_id }
    }

    pub fn id(&self) -> &str {
        &self.trace_id
    }

    /// Open a span under this trace.
    pub fn span(&self, name: &str, input: Option<&str>) -> SpanHandle {
        let span_id = new_id();
        let start_time = now_iso();
        let mut body = serde_json::json!({
            "id": span_id,
            "traceId": self.trace_id,
            "name": name,
            "startTime": start_time,
        });
        if let Some(inp) = input {
            body["input"] = serde_json::json!(inp);
        }
        self.client
            .ingest(vec![ingestion_event("span-create", body)]);
        SpanHandle {
            client: self.client.clone(),
            trace_id: self.trace_id.clone(),
            span_id,
        }
    }

    /// Open a generation (LLM call) under this trace.
    pub fn generation(&self, name: &str, model: &str, input: Option<&str>) -> GenerationHandle {
        let gen_id = new_id();
        let start_time = now_iso();
        let mut body = serde_json::json!({
            "id": gen_id,
            "traceId": self.trace_id,
            "name": name,
            "model": model,
            "startTime": start_time,
        });
        if let Some(inp) = input {
            body["input"] = serde_json::json!(inp);
        }
        self.client
            .ingest(vec![ingestion_event("generation-create", body)]);
        GenerationHandle {
            client: self.client.clone(),
            trace_id: self.trace_id.clone(),
            gen_id,
        }
    }

    /// End this trace (upsert with output).
    pub fn end(self, output: Option<&str>) {
        let mut body = serde_json::json!({
            "id": self.trace_id,
            "timestamp": now_iso(),
        });
        if let Some(out) = output {
            body["output"] = serde_json::json!(out);
        }
        self.client
            .ingest(vec![ingestion_event("trace-create", body)]);
    }
}

// ---------------------------------------------------------------------------
// SpanHandle
// ---------------------------------------------------------------------------

pub struct SpanHandle {
    client: Arc<LangfuseClient>,
    trace_id: String,
    span_id: String,
}

impl SpanHandle {
    pub fn end(self, output: Option<&str>) {
        let mut body = serde_json::json!({
            "id": self.span_id,
            "traceId": self.trace_id,
            "endTime": now_iso(),
        });
        if let Some(out) = output {
            body["output"] = serde_json::json!(out);
        }
        self.client
            .ingest(vec![ingestion_event("span-update", body)]);
    }
}

// ---------------------------------------------------------------------------
// GenerationHandle
// ---------------------------------------------------------------------------

pub struct GenerationHandle {
    client: Arc<LangfuseClient>,
    trace_id: String,
    gen_id: String,
}

impl GenerationHandle {
    pub fn end(self, output: Option<&str>, prompt_tokens: u64, completion_tokens: u64) {
        let mut body = serde_json::json!({
            "id": self.gen_id,
            "traceId": self.trace_id,
            "endTime": now_iso(),
            "usage": {
                "promptTokens": prompt_tokens,
                "completionTokens": completion_tokens,
            },
        });
        if let Some(out) = output {
            body["output"] = serde_json::json!(out);
        }
        self.client
            .ingest(vec![ingestion_event("generation-update", body)]);
    }
}

// ---------------------------------------------------------------------------
// Shared type alias
// ---------------------------------------------------------------------------

pub type SharedLangfuse = Option<Arc<LangfuseClient>>;
