Perform a security review of the Emberloom codebase. Focus on these areas:

1. **Secrets handling**: API keys in config, env vars, logging — are secrets ever leaked to logs/output? Check all structs that derive Debug and contain sensitive fields.
2. **Input validation**: User input from Telegram, LLM responses parsed as JSON, tool parameters
3. **Injection risks**: Shell command injection via Docker exec, path traversal in file tools, shell escaping in exec_with_stdin
4. **Network security**: HTTP vs HTTPS, TLS validation, auth token handling
5. **Docker security**: Container escape risks, mount permissions, resource limits, user/capabilities, no_new_privileges, PID limits
6. **LLM-specific risks**: Prompt injection, tool call manipulation, sensitive data in prompts, agent selection manipulation
7. **Error handling**: Are internal errors leaking details to Telegram users?
8. **Access control**: Telegram allowed_chats enforcement, auto-approve logic, rate limiting
9. **Dependencies**: Deprecated crates, known risky patterns

Read ALL source files in src/ and config files. Report specific file:line references for each finding.

Categorize findings as:
- CRITICAL: Exploitable now
- HIGH: Significant risk
- MEDIUM: Should fix
- LOW: Best practice improvement

End with a prioritized remediation plan.
