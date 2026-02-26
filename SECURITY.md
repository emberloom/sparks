# Security Policy

## Supported Version

Security fixes are provided for the current `main` branch and latest tagged release.

## Reporting a Vulnerability

Please do not open public issues for suspected vulnerabilities.

Use one of these channels:

1. Open a private GitHub Security Advisory draft for this repository.
2. If that is unavailable, open a minimal issue asking maintainers for a private contact path (without exploit details).

Include:

- affected commit/tag
- impact summary
- reproduction steps
- suggested mitigation (if known)

## Secret Handling

- keep credentials out of `config.toml`
- prefer environment variables or a gitignored `.env` file
