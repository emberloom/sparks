# Rich Context Injection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a ticket from GitHub or Linear spawns an `AutonomousTask`, inject the full issue body, comments, and PR diff into the task context — not just the title — so sparks have the complete picture before they start.

**Architecture:** `TicketProvider` gains an optional `fetch_full_context()` method. `ExternalTicket.to_autonomous_task()` uses this to embed rich context into the `AutonomousTask.context` string at intake time. `context_budget.rs` trims if over a configurable char cap. GitHub and Linear are the priority providers.

> **Spec note:** The spec lists `src/strategy/mod.rs` as a touchpoint for a new `TaskContract.rich_context: Option<String>` field. This plan intentionally skips that struct change and instead appends rich context directly to the existing `AutonomousTask.context` string at intake time. The effect is identical (context reaches the LLM via `TaskContract.context`), and this approach avoids a struct change that would require updating all `TaskContract` construction sites. If a separate field is desired later for observability, it can be added incrementally.

**Tech Stack:** Rust, reqwest, async-trait

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/ticket_intake/provider.rs` | Modify | Add `fetch_full_context()` default method + `TicketContext` struct |
| `src/ticket_intake/github.rs` | Modify | Implement: fetch issue comments (and PR diff for PRs) |
| `src/ticket_intake/linear.rs` | Modify | Implement: fetch issue comments |
| `src/config.rs` | Modify | Add `inject_full_context` and `rich_context_char_cap` to `TicketIntakeConfig` |
| `src/ticket_intake/mod.rs` | Modify | Pass config flag through to provider call |

---

## Task 1: Add `TicketContext` and extend `TicketProvider` trait

**Files:**
- Modify: `src/ticket_intake/provider.rs`

- [ ] **Step 1: Write failing test**

Add to `src/ticket_intake/provider.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_context_format_includes_all_sections() {
        let ctx = TicketContext {
            comments: vec!["comment one".into(), "comment two".into()],
            diff_summary: Some("+ added line\n- removed line".into()),
        };
        let formatted = ctx.format(4000);
        assert!(formatted.contains("comment one"));
        assert!(formatted.contains("+ added line"));
    }

    #[test]
    fn ticket_context_format_trims_to_char_cap() {
        let long_comment = "x".repeat(5000);
        let ctx = TicketContext {
            comments: vec![long_comment],
            diff_summary: None,
        };
        let formatted = ctx.format(1000);
        assert!(formatted.len() <= 1100); // small headroom for section headers
    }
}
```

- [ ] **Step 2: Run test — expect compile failure**

```bash
cargo test -p sparks ticket_intake::provider::tests 2>&1 | head -20
```

- [ ] **Step 3: Add `TicketContext` struct and extend the trait**

In `src/ticket_intake/provider.rs`, add before the `TicketProvider` trait:

```rust
/// Rich supplementary context fetched on demand (comments, diffs).
#[derive(Debug, Default)]
pub struct TicketContext {
    pub comments: Vec<String>,
    /// For PRs: unified diff summary (first N lines).
    pub diff_summary: Option<String>,
}

impl TicketContext {
    /// Format into a markdown block, trimming to `char_cap` characters.
    ///
    /// Trim order when over budget: comments first (drop oldest), then diff.
    /// Description is never trimmed here — it already lives in the task goal.
    pub fn format(&self, char_cap: usize) -> String {
        // Attempt to fit everything; if over budget, try dropping comments.
        let full = self.format_inner(&self.comments, self.diff_summary.as_deref());
        if full.len() <= char_cap {
            return full;
        }

        // Drop comments progressively from the end until we fit.
        for keep in (0..self.comments.len()).rev() {
            let partial = self.format_inner(&self.comments[..keep], self.diff_summary.as_deref());
            if partial.len() <= char_cap {
                return partial;
            }
        }

        // Still too long — drop diff too, keep only what fits.
        let no_diff = self.format_inner(&[], None);
        if no_diff.len() <= char_cap {
            return no_diff;
        }

        // Hard truncate as last resort.
        let mut s = no_diff;
        s.truncate(char_cap);
        s
    }

    fn format_inner(&self, comments: &[String], diff: Option<&str>) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !comments.is_empty() {
            parts.push("### Comments\n".to_string());
            for c in comments {
                parts.push(format!("- {}\n", c));
            }
        }
        if let Some(d) = diff {
            parts.push(format!("### Diff\n```diff\n{}\n```\n", d));
        }
        parts.join("")
    }
}
```

Add to the `TicketProvider` trait (in the `pub trait TicketProvider` block):

```rust
/// Fetch rich supplementary context for a ticket (comments, diff).
/// Providers that don't implement this return an empty `TicketContext`.
async fn fetch_full_context(&self, _ticket: &ExternalTicket) -> Result<TicketContext> {
    Ok(TicketContext::default())
}
```

- [ ] **Step 4: Run test — expect pass**

```bash
cargo test -p sparks ticket_intake::provider::tests 2>&1
```

- [ ] **Step 5: Check compile**

```bash
cargo check 2>&1 | head -20
```

- [ ] **Step 6: Commit**

```bash
git add src/ticket_intake/provider.rs
git commit -m "feat(ticket-intake): add TicketContext and fetch_full_context trait method"
```

---

## Task 2: Implement `fetch_full_context` for GitHub

**Files:**
- Modify: `src/ticket_intake/github.rs`

- [ ] **Step 1: Write failing test**

Add to bottom of `src/ticket_intake/github.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_comment_body_strips_whitespace() {
        let raw = "  some comment body  \n";
        assert_eq!(raw.trim(), "some comment body");
    }

    #[test]
    fn diff_truncation_keeps_first_lines() {
        let diff = (0..200).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let truncated = truncate_diff(&diff, 100);
        assert!(truncated.lines().count() <= 100);
        assert!(truncated.starts_with("line 0"));
    }
}
```

- [ ] **Step 2: Run test — expect compile failure**

```bash
cargo test -p sparks ticket_intake::github::tests 2>&1 | head -20
```

- [ ] **Step 3: Add helper and implement `fetch_full_context`**

Add to `src/ticket_intake/github.rs`:

```rust
fn truncate_diff(diff: &str, max_lines: usize) -> String {
    diff.lines().take(max_lines).collect::<Vec<_>>().join("\n")
}

#[derive(Deserialize)]
struct GhComment {
    body: String,
}
```

Add to the `impl TicketProvider for GitHubProvider` block:

```rust
async fn fetch_full_context(&self, ticket: &ExternalTicket) -> Result<TicketContext> {
    let Some(number) = ticket.number.as_ref() else {
        return Ok(TicketContext::default());
    };

    // Fetch comments
    let comments_url = format!(
        "{}/repos/{}/issues/{}/comments",
        self.api_base.trim_end_matches('/'),
        self.repo,
        number
    );
    let comments: Vec<GhComment> = self
        .client
        .get(&comments_url)
        .header(AUTHORIZATION, format!("Bearer {}", self.token))
        .header(ACCEPT, "application/vnd.github+json")
        .query(&[("per_page", "20")])
        .send()
        .await
        .map_err(|e| SparksError::Tool(format!("GitHub comments request failed: {}", e)))?
        .error_for_status()
        .map_err(|e| SparksError::Tool(format!("GitHub comments error: {}", e)))?
        .json()
        .await
        .map_err(|e| SparksError::Tool(format!("GitHub comments parse failed: {}", e)))?;

    let comment_bodies: Vec<String> = comments
        .into_iter()
        .map(|c| c.body.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(TicketContext {
        comments: comment_bodies,
        diff_summary: None, // PR diff support is a follow-up
    })
}
```

- [ ] **Step 4: Run test — expect pass**

```bash
cargo test -p sparks ticket_intake::github::tests 2>&1
```

- [ ] **Step 5: Commit**

```bash
git add src/ticket_intake/github.rs
git commit -m "feat(ticket-intake): implement fetch_full_context for GitHubProvider"
```

---

## Task 3: Implement `fetch_full_context` for Linear

**Files:**
- Modify: `src/ticket_intake/linear.rs`
- Read first: `src/ticket_intake/linear.rs` to find the existing `reqwest::Client` and auth pattern

- [ ] **Step 1: Read `linear.rs` to understand existing HTTP client setup**

```bash
head -80 src/ticket_intake/linear.rs
```

- [ ] **Step 2: Write test**

Add to bottom of `src/ticket_intake/linear.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn linear_comment_trimming() {
        let raw = "   text   ";
        assert_eq!(raw.trim(), "text");
    }
}
```

- [ ] **Step 3: Implement `fetch_full_context` for Linear**

In `src/ticket_intake/linear.rs`, add to `impl TicketProvider for LinearProvider`:

```rust
async fn fetch_full_context(&self, ticket: &ExternalTicket) -> Result<TicketContext> {
    // Linear GraphQL: fetch comments for an issue by external_id
    let query = format!(
        r#"{{ "query": "{{ issue(id: \"{}\") {{ comments {{ nodes {{ body }} }} }} }}" }}"#,
        ticket.external_id
    );

    let resp: serde_json::Value = self
        .client
        .post("https://api.linear.app/graphql")
        .header("Authorization", &self.api_key)
        .header("Content-Type", "application/json")
        .body(query)
        .send()
        .await
        .map_err(|e| SparksError::Tool(format!("Linear comments request failed: {}", e)))?
        .json()
        .await
        .map_err(|e| SparksError::Tool(format!("Linear comments parse failed: {}", e)))?;

    let comments = resp["data"]["issue"]["comments"]["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| n["body"].as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(TicketContext {
        comments,
        diff_summary: None,
    })
}
```

Note: check `linear.rs` for the actual field names (`api_key`, `client`) — adjust if they differ.

- [ ] **Step 4: Compile check**

```bash
cargo check 2>&1 | head -20
```

- [ ] **Step 5: Commit**

```bash
git add src/ticket_intake/linear.rs
git commit -m "feat(ticket-intake): implement fetch_full_context for LinearProvider"
```

---

## Task 4: Add config flags and wire into ticket intake loop

**Files:**
- Modify: `src/config.rs`
- Modify: `src/ticket_intake/mod.rs`

- [ ] **Step 1: Add config fields**

In `src/config.rs`, find `TicketIntakeConfig` and add:

```rust
/// Fetch full issue context (comments, PR diff) and inject into task context.
#[serde(default)]
pub inject_full_context: bool,

/// Char cap for injected rich context block (default: 4000).
#[serde(default = "default_rich_context_char_cap")]
pub rich_context_char_cap: usize,
```

Add the default function near other defaults:

```rust
fn default_rich_context_char_cap() -> usize { 4_000 }
```

- [ ] **Step 2: Write test for config deserialization**

Add to `src/config.rs` tests (find or create `#[cfg(test)]` block):

```rust
#[test]
fn ticket_intake_config_defaults() {
    let config: TicketIntakeConfig = toml::from_str("").unwrap();
    assert!(!config.inject_full_context); // off by default
    assert_eq!(config.rich_context_char_cap, 4_000);
}
```

- [ ] **Step 3: Run test**

```bash
cargo test -p sparks config 2>&1 | grep ticket
```

- [ ] **Step 4: Wire in `ticket_intake/mod.rs`**

In `src/ticket_intake/mod.rs`, find the polling loop where `provider.poll()` is called and tickets are converted to `AutonomousTask`. After obtaining the `ExternalTicket` and before calling `ticket.to_autonomous_task()`, conditionally fetch rich context and append it:

```rust
let mut task = ticket.to_autonomous_task();

if knobs.read().map(|k| k.inject_full_context).unwrap_or(false)
    || config.inject_full_context
{
    match provider.fetch_full_context(&ticket).await {
        Ok(ctx) => {
            let formatted = ctx.format(config.rich_context_char_cap);
            if !formatted.is_empty() {
                task.context.push_str("\n\n### Full Ticket Context\n");
                task.context.push_str(&formatted);
            }
        }
        Err(e) => {
            tracing::warn!("fetch_full_context failed, continuing without it: {}", e);
        }
    }
}
```

Note: If `inject_full_context` isn't on `SharedKnobs`, use only the config flag. Adjust accordingly based on what `knobs` exposes.

- [ ] **Step 5: Compile check**

```bash
cargo check 2>&1 | head -30
```

- [ ] **Step 6: Run all tests**

```bash
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 7: Commit**

```bash
git add src/config.rs src/ticket_intake/mod.rs
git commit -m "feat(ticket-intake): wire inject_full_context config flag into polling loop"
```

---

## Task 5: Update `config.example.toml`

**Files:**
- Modify: `config.example.toml`

- [ ] **Step 1: Add new fields to the `[ticket_intake]` section**

Find the existing `[ticket_intake]` section and add:

```toml
# Fetch full issue context (comments, PR diff) and inject into task context.
# Increases token usage per task but significantly improves task quality.
# inject_full_context = false

# Maximum characters to inject from rich ticket context (default: 4000).
# rich_context_char_cap = 4000
```

- [ ] **Step 2: Commit**

```bash
git add config.example.toml
git commit -m "docs: add rich context injection config fields to config.example.toml"
```
