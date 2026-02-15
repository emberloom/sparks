# Secrets with Vaultwarden (Local)

This keeps Athena credentials out of `config.toml`.

## 1) Store secrets in Vaultwarden

Create password entries (names can be changed):

- `athena/openrouter_api_key`
- `athena/opencode_api_key`
- `athena/github_token`
- `athena/langfuse_public_key`
- `athena/langfuse_secret_key`
- `athena/telegram_token`
- `athena/stt_api_key`

## 2) Unlock with Bitwarden CLI

```bash
bw login --apikey
export BW_SESSION="$(bw unlock --raw)"
```

If you self-host Vaultwarden locally with TLS, point CLI to it first:

```bash
export NODE_EXTRA_CA_CERTS="$HOME/.vaultwarden-data/ssl/local-ca.crt"
bw logout
bw config server https://localhost:9443
```

Open the local web vault at:

```text
https://localhost:9443
```

## 3) Run Athena with runtime secret injection

```bash
./scripts/athena-with-vaultwarden.sh ./target/debug/athena doctor
```

The wrapper prefers Vaultwarden values over pre-existing shell env vars, so rotated keys take effect immediately.

Or default to launching Athena directly:

```bash
./scripts/athena-with-vaultwarden.sh
```

## 4) Keep config clean

- Remove plaintext tokens from `config.toml`.
- Keep provider/token fields unset and rely on env vars.
- Rotate any key that was previously committed, logged, or shared.

## Optional item name overrides

Set any of these when your Vaultwarden item names differ:

- `ATHENA_BW_ITEM_OPENROUTER`
- `ATHENA_BW_ITEM_ZEN`
- `ATHENA_BW_ITEM_GITHUB`
- `ATHENA_BW_ITEM_LANGFUSE_PUBLIC`
- `ATHENA_BW_ITEM_LANGFUSE_SECRET`
- `ATHENA_BW_ITEM_TELEGRAM`
- `ATHENA_BW_ITEM_STT`
