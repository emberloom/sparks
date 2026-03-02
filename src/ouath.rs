use std::path::PathBuf;
use std::process::Command;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};

use crate::config::OuathConfig;
use crate::error::{AthenaError, Result};

const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEFAULT_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEFAULT_SCOPES: &str = "openid profile email offline_access";
const DEFAULT_CALLBACK_TIMEOUT_SECS: u64 = 300;
const DEFAULT_ACCOUNT_URL: &str = "https://api.openai.com/auth";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OuathTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub chatgpt_account_id: Option<String>,
}

impl OuathTokens {
    pub fn expired(&self, skew_secs: i64) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.expires_at - skew_secs <= now
    }
}

pub struct OuathAuth {
    config: OuathConfig,
    client: Client,
    token_path: PathBuf,
    cached: RwLock<Option<OuathTokens>>,
}

impl OuathAuth {
    pub fn new(config: OuathConfig) -> Self {
        let token_path = expand_path(&config.auth_file);
        Self {
            config,
            client: Client::new(),
            token_path,
            cached: RwLock::new(None),
        }
    }

    pub async fn load_tokens(&self) -> Result<Option<OuathTokens>> {
        {
            let read = self.cached.read().await;
            if read.is_some() {
                return Ok(read.clone());
            }
        }

        if !self.token_path.exists() {
            return Ok(None);
        }

        let contents = std::fs::read_to_string(&self.token_path).map_err(|e| {
            AthenaError::Config(format!(
                "Failed to read Ouath token file {}: {}",
                self.token_path.display(),
                e
            ))
        })?;
        let tokens: OuathTokens = serde_json::from_str(&contents).map_err(|e| {
            AthenaError::Config(format!(
                "Failed to parse Ouath token file {}: {}",
                self.token_path.display(),
                e
            ))
        })?;
        let mut write = self.cached.write().await;
        *write = Some(tokens.clone());
        Ok(Some(tokens))
    }

    pub async fn save_tokens(&self, tokens: &OuathTokens) -> Result<()> {
        if let Some(parent) = self.token_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AthenaError::Config(format!(
                    "Failed to create Ouath token dir {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let body = serde_json::to_string_pretty(tokens).map_err(|e| {
            AthenaError::Internal(format!("Failed to serialize Ouath tokens: {}", e))
        })?;
        std::fs::write(&self.token_path, body).map_err(|e| {
            AthenaError::Config(format!(
                "Failed to write Ouath token file {}: {}",
                self.token_path.display(),
                e
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.token_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| {
                    AthenaError::Config(format!(
                        "Failed to set permissions on Ouath token file {}: {}",
                        self.token_path.display(),
                        e
                    ))
                })?;
        }

        let mut write = self.cached.write().await;
        *write = Some(tokens.clone());
        Ok(())
    }

    pub async fn ensure_valid_tokens(&self) -> Result<OuathTokens> {
        let tokens = self.load_tokens().await?.ok_or_else(|| {
            AthenaError::Config("Ouath not authenticated. Run `athena ouath login`.".into())
        })?;

        if !tokens.expired(60) {
            return Ok(tokens);
        }

        let refreshed = self
            .refresh_tokens(&tokens.refresh_token, tokens.chatgpt_account_id.as_deref())
            .await?;
        self.save_tokens(&refreshed).await?;
        Ok(refreshed)
    }

    pub async fn force_refresh(&self) -> Result<OuathTokens> {
        let tokens = self.load_tokens().await?.ok_or_else(|| {
            AthenaError::Config("Ouath not authenticated. Run `athena ouath login`.".into())
        })?;
        let refreshed = self
            .refresh_tokens(&tokens.refresh_token, tokens.chatgpt_account_id.as_deref())
            .await?;
        self.save_tokens(&refreshed).await?;
        Ok(refreshed)
    }

    pub async fn login_interactive(&self) -> Result<OuathTokens> {
        let client_id = oauth_client_id();
        let auth_url = oauth_auth_url();
        let token_url = oauth_token_url();
        let scopes = oauth_scopes();
        let redirect_uri = oauth_redirect_uri(&self.config);

        let (code_verifier, code_challenge) = pkce_pair();
        let state = random_state();

        let authorize_url = build_authorize_url(
            &auth_url,
            &client_id,
            &redirect_uri,
            &scopes,
            &code_challenge,
            &state,
        )?;

        eprintln!("Open this URL to authenticate:\n{}", authorize_url);
        try_open_browser(&authorize_url);

        let auth_code = wait_for_auth_code(&redirect_uri, &state).await?;

        let tokens = exchange_code_for_tokens(
            &self.client,
            &token_url,
            &client_id,
            &redirect_uri,
            &auth_code,
            &code_verifier,
        )
        .await?;

        let resolved = self.resolve_account_id(tokens).await?;
        self.save_tokens(&resolved).await?;
        Ok(resolved)
    }

    pub async fn logout(&self) -> Result<()> {
        if self.token_path.exists() {
            std::fs::remove_file(&self.token_path).map_err(|e| {
                AthenaError::Config(format!(
                    "Failed to remove Ouath token file {}: {}",
                    self.token_path.display(),
                    e
                ))
            })?;
        }
        let mut write = self.cached.write().await;
        *write = None;
        Ok(())
    }

    async fn refresh_tokens(
        &self,
        refresh_token: &str,
        old_account_id: Option<&str>,
    ) -> Result<OuathTokens> {
        let client_id = oauth_client_id();
        let token_url = oauth_token_url();
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id.as_str()),
        ];

        let resp = self.client.post(token_url).form(&params).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AthenaError::Llm(format!(
                "Ouath refresh failed ({}): {}",
                status, body
            )));
        }

        let body: TokenResponse = resp.json().await?;
        let expires_at = chrono::Utc::now().timestamp() + body.expires_in;
        let access_token = body.access_token;
        let refresh_token = body
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string());

        let tokens = OuathTokens {
            access_token,
            refresh_token,
            expires_at,
            chatgpt_account_id: None,
        };
        let mut resolved = self.resolve_account_id(tokens).await?;
        if resolved.chatgpt_account_id.is_none() {
            resolved.chatgpt_account_id = old_account_id.map(String::from);
        }
        Ok(resolved)
    }

    async fn resolve_account_id(&self, mut tokens: OuathTokens) -> Result<OuathTokens> {
        if tokens.chatgpt_account_id.is_none() {
            tokens.chatgpt_account_id = extract_account_id(&tokens.access_token);
            if tokens.chatgpt_account_id.is_none() {
                if let Ok(Some(id)) = self.fetch_account_id(&tokens.access_token).await {
                    tokens.chatgpt_account_id = Some(id);
                }
            }
        }
        Ok(tokens)
    }

    async fn fetch_account_id(&self, access_token: &str) -> Result<Option<String>> {
        let resp = self
            .client
            .get(DEFAULT_ACCOUNT_URL)
            .bearer_auth(access_token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: Value = resp.json().await?;
        let id = body
            .get("chatgpt_account_id")
            .and_then(|v| v.as_str())
            .or_else(|| body.get("account_id").and_then(|v| v.as_str()))
            .map(|s| s.to_string());
        Ok(id)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

fn oauth_client_id() -> String {
    std::env::var("OUATH_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.into())
}

fn oauth_auth_url() -> String {
    std::env::var("OUATH_AUTH_URL").unwrap_or_else(|_| DEFAULT_AUTH_URL.into())
}

fn oauth_token_url() -> String {
    std::env::var("OUATH_TOKEN_URL").unwrap_or_else(|_| DEFAULT_TOKEN_URL.into())
}

fn oauth_scopes() -> String {
    std::env::var("OUATH_SCOPES").unwrap_or_else(|_| DEFAULT_SCOPES.into())
}

fn oauth_redirect_uri(config: &OuathConfig) -> String {
    if let Ok(val) = std::env::var("OUATH_REDIRECT_URI") {
        return val;
    }
    config
        .redirect_uri
        .clone()
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.into())
}

fn expand_path(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(raw)
}

fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());
    (verifier, challenge)
}

fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn build_authorize_url(
    auth_url: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &str,
    code_challenge: &str,
    state: &str,
) -> Result<String> {
    let mut url = url::Url::parse(auth_url)
        .map_err(|e| AthenaError::Config(format!("Invalid Ouath auth URL {}: {}", auth_url, e)))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scopes)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("codex_cli_simplified_flow", "true");
    Ok(url.to_string())
}

async fn wait_for_auth_code(redirect_uri: &str, expected_state: &str) -> Result<String> {
    let url = url::Url::parse(redirect_uri).map_err(|e| {
        AthenaError::Config(format!("Invalid redirect URI {}: {}", redirect_uri, e))
    })?;
    let host = url.host_str().unwrap_or("127.0.0.1");
    let port = url.port().unwrap_or(1455);
    let listener = TcpListener::bind((host, port)).await.map_err(|e| {
        AthenaError::Config(format!(
            "Failed to bind Ouath callback listener on {}:{}: {}",
            host, port, e
        ))
    })?;

    let (mut socket, _) = timeout(
        Duration::from_secs(DEFAULT_CALLBACK_TIMEOUT_SECS),
        listener.accept(),
    )
    .await
    .map_err(|_| AthenaError::Timeout(DEFAULT_CALLBACK_TIMEOUT_SECS))?
    .map_err(|e| AthenaError::Config(format!("Ouath callback listener failed: {}", e)))?;

    let mut buf = [0u8; 4096];
    let n = socket.read(&mut buf).await.map_err(|e| {
        AthenaError::Config(format!("Failed to read Ouath callback request: {}", e))
    })?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let parsed = url::Url::parse(&format!("http://localhost{}", path))
        .map_err(|e| AthenaError::Config(format!("Failed to parse Ouath callback URL: {}", e)))?;
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        if k == "code" {
            code = Some(v.to_string());
        } else if k == "state" {
            state = Some(v.to_string());
        }
    }

    let Some(code) = code else {
        let body = "<html><body><h3>Ouath login failed.</h3><p>Missing authorization code.</p></body></html>";
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = socket.write_all(response.as_bytes()).await;
        return Err(AthenaError::Config(
            "Ouath callback missing authorization code".into(),
        ));
    };
    if state.as_deref() != Some(expected_state) {
        let body = "<html><body><h3>Ouath login failed.</h3><p>State mismatch (possible CSRF).</p></body></html>";
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = socket.write_all(response.as_bytes()).await;
        return Err(AthenaError::Config("Ouath callback state mismatch".into()));
    }

    let body =
        "<html><body><h3>Ouath login complete.</h3><p>You can close this window.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;

    Ok(code)
}

async fn exchange_code_for_tokens(
    client: &Client,
    token_url: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> Result<OuathTokens> {
    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", code_verifier),
        ("redirect_uri", redirect_uri),
    ];

    let resp = client.post(token_url).form(&params).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AthenaError::Llm(format!(
            "Ouath token exchange failed ({}): {}",
            status, body
        )));
    }

    let body: TokenResponse = resp.json().await?;
    let expires_at = chrono::Utc::now().timestamp() + body.expires_in;

    Ok(OuathTokens {
        access_token: body.access_token,
        refresh_token: body.refresh_token.ok_or_else(|| {
            AthenaError::Config("Ouath token response missing refresh_token".into())
        })?,
        expires_at,
        chatgpt_account_id: None,
    })
}

fn extract_account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            value
                .get("https://api.openai.com/auth")
                .and_then(|v| v.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

fn try_open_browser(url: &str) {
    if Command::new("open").arg(url).status().is_ok() {
        return;
    }
    let _ = Command::new("xdg-open").arg(url).status();
}
