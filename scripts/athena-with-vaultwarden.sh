#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULT_BIN="$ROOT_DIR/target/debug/athena"
DEFAULT_LOCAL_CA="$HOME/.vaultwarden-data/ssl/local-ca.crt"

if ! command -v bw >/dev/null 2>&1; then
  echo "Bitwarden CLI (bw) is required. Install it first." >&2
  exit 1
fi

# Bitwarden CLI (node) may need explicit CA path for self-hosted local TLS.
if [[ -z "${NODE_EXTRA_CA_CERTS:-}" && -f "$DEFAULT_LOCAL_CA" ]]; then
  export NODE_EXTRA_CA_CERTS="$DEFAULT_LOCAL_CA"
fi

if [[ -z "${BW_SESSION:-}" ]]; then
  cat >&2 <<'EOF'
BW_SESSION is not set.
Unlock first, then re-run:
  export BW_SESSION="$(bw unlock --raw)"
EOF
  exit 1
fi

read_secret() {
  local item="$1"
  bw get password "$item" 2>/dev/null || true
}

set_from_vault_or_env() {
  local env_name="$1"
  local item_name="$2"
  local value=""

  value="$(read_secret "$item_name")"
  if [[ -n "$value" ]]; then
    export "$env_name=$value"
    return 0
  fi

  # Optional fallback: keep existing env value only when vault item is absent.
  if [[ -n "${!env_name:-}" ]]; then
    return 0
  fi
}

# Override these item names with env vars if your Vaultwarden naming differs.
set_from_vault_or_env OPENROUTER_API_KEY "${ATHENA_BW_ITEM_OPENROUTER:-athena/openrouter_api_key}"
set_from_vault_or_env OPENCODE_API_KEY "${ATHENA_BW_ITEM_ZEN:-athena/opencode_api_key}"
set_from_vault_or_env GH_TOKEN "${ATHENA_BW_ITEM_GITHUB:-athena/github_token}"
set_from_vault_or_env LANGFUSE_PUBLIC_KEY "${ATHENA_BW_ITEM_LANGFUSE_PUBLIC:-athena/langfuse_public_key}"
set_from_vault_or_env LANGFUSE_SECRET_KEY "${ATHENA_BW_ITEM_LANGFUSE_SECRET:-athena/langfuse_secret_key}"
set_from_vault_or_env ATHENA_TELEGRAM_TOKEN "${ATHENA_BW_ITEM_TELEGRAM:-athena/telegram_token}"
set_from_vault_or_env ATHENA_STT_API_KEY "${ATHENA_BW_ITEM_STT:-athena/stt_api_key}"

if [[ $# -eq 0 ]]; then
  exec "$DEFAULT_BIN"
else
  exec "$@"
fi
