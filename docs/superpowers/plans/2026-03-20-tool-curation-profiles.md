# Tool Curation Profiles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Named tool profiles in `[tool_profiles]` can be referenced by ghost configs via `profile = "name"`, eliminating copy-paste of tool lists across ghosts as the MCP registry grows.

**Architecture:** A new `[tool_profiles]` TOML section maps profile names to `Vec<String>` tool lists. `GhostConfig` gains an optional `profile` field. `src/profiles.rs` resolves the reference at startup, merging the profile's tool list into the ghost's `tools` field. `doctor.rs` validates that all referenced profiles exist and all listed tools are registered.

**Tech Stack:** Rust, serde, toml

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/config.rs` | Modify | Add `ToolProfiles` type to `Config`; add `profile` field to `GhostConfig` |
| `src/profiles.rs` | Modify | Resolve `profile` references during ghost loading |
| `src/doctor.rs` | Modify | Validate profile existence and tool registration |
| `config.example.toml` | Modify | Add example `[tool_profiles]` section |

---

## Task 1: Add `ToolProfiles` to `Config` and `profile` to `GhostConfig`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write failing test**

Add to `src/config.rs` tests:

```rust
#[cfg(test)]
mod tool_profile_tests {
    use super::*;

    #[test]
    fn tool_profiles_deserialize_correctly() {
        let toml = r#"
[tool_profiles]
researcher = ["web_search", "file_read", "memory_search"]
devops = ["shell", "docker", "git"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.tool_profiles.get("researcher"),
            Some(&vec!["web_search".to_string(), "file_read".to_string(), "memory_search".to_string()])
        );
        assert_eq!(config.tool_profiles.get("devops").map(|v| v.len()), Some(3));
    }

    #[test]
    fn ghost_config_profile_field_deserializes() {
        let toml = r#"
[[ghosts]]
name = "researcher"
description = "Research ghost"
profile = "researcher"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ghosts[0].profile.as_deref(), Some("researcher"));
    }

    #[test]
    fn tool_profiles_default_to_empty_map() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.tool_profiles.is_empty());
    }
}
```

- [ ] **Step 2: Run test — expect compile failure**

```bash
cargo test -p sparks tool_profile_tests 2>&1 | head -20
```

- [ ] **Step 3: Add `ToolProfiles` type to `config.rs`**

Add a type alias near the top of `config.rs`:

```rust
/// Named tool lists — map of profile_name → Vec<tool_name>.
pub type ToolProfiles = std::collections::HashMap<String, Vec<String>>;
```

Add to `Config` struct:

```rust
#[serde(default)]
pub tool_profiles: ToolProfiles,
```

Add `profile` field to `GhostConfig`:

```rust
/// Optional named tool profile. Resolved at startup by `profiles.rs`.
/// If set, the profile's tool list is merged into `tools` (profile wins for
/// any tool not already in `tools`; inline `tools` list takes precedence).
#[serde(default)]
pub profile: Option<String>,
```

- [ ] **Step 4: Run tests — expect pass**

```bash
cargo test -p sparks tool_profile_tests 2>&1
```

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(profiles): add ToolProfiles config type and GhostConfig.profile field"
```

---

## Task 2: Resolve profile references in `profiles.rs`

**Files:**
- Modify: `src/profiles.rs`

**Context:** `src/profiles.rs` loads ghost profiles from `~/.sparks/ghosts/*.toml` and merges them with inline `[[ghosts]]` config entries. Read the existing `load_ghosts()` function to understand where to add resolution.

- [ ] **Step 1: Read `profiles.rs` to find the load function**

```bash
grep -n "pub fn\|fn load" src/profiles.rs | head -20
```

- [ ] **Step 2: Write failing test**

Add to `src/profiles.rs`:

```rust
#[cfg(test)]
mod profile_resolution_tests {
    use super::*;
    use std::collections::HashMap;

    fn make_profiles() -> crate::config::ToolProfiles {
        let mut p = HashMap::new();
        p.insert("researcher".to_string(), vec!["web_search".to_string(), "memory_search".to_string()]);
        p
    }

    fn make_ghost(tools: Vec<String>, profile: Option<&str>) -> crate::config::GhostConfig {
        // GhostConfig does not derive Default. Construct manually with required fields.
        // Required fields (no #[serde(default)]): name, description.
        // All other fields have serde defaults and are Option or Vec.
        crate::config::GhostConfig {
            name: "test".to_string(),
            description: "test ghost".to_string(),
            tools,
            mounts: vec![],
            role: crate::config::GhostRole::default(),
            strategy: "react".to_string(),
            soul_file: None,
            skill_file: None,
            skill_files: vec![],
            soul: None,
            skill: None,
            image: None,
            profile: profile.map(|s| s.to_string()),
        }
    }

    #[test]
    fn resolve_profile_sets_tools_when_tools_empty() {
        let profiles = make_profiles();
        let mut ghost = make_ghost(vec![], Some("researcher"));
        resolve_ghost_profile(&mut ghost, &profiles);
        assert_eq!(ghost.tools, vec!["web_search", "memory_search"]);
    }

    #[test]
    fn resolve_profile_inline_tools_take_precedence() {
        let profiles = make_profiles();
        let mut ghost = make_ghost(vec!["custom_tool".to_string()], Some("researcher"));
        resolve_ghost_profile(&mut ghost, &profiles);
        // Inline list wins — profile tools appended only if not present
        assert!(ghost.tools.contains(&"custom_tool".to_string()));
        assert!(ghost.tools.contains(&"web_search".to_string()));
    }

    #[test]
    fn resolve_profile_unknown_profile_logs_warning_and_continues() {
        let profiles = make_profiles();
        let mut ghost = make_ghost(vec![], Some("nonexistent"));
        // Should not panic
        resolve_ghost_profile(&mut ghost, &profiles);
        // tools unchanged
        assert!(ghost.tools.is_empty());
    }
}
```

Note: The field list in `make_ghost()` must match `GhostConfig` exactly. If the struct has changed (e.g., new fields added), the compiler will report a missing field error — add the missing fields with appropriate zero/empty values.

- [ ] **Step 3: Add `resolve_ghost_profile` function to `profiles.rs`**

```rust
/// Merge a named tool profile into a ghost's `tools` list.
/// Inline `tools` entries always take precedence over profile entries.
/// If the profile name is not found, log a warning and leave `tools` unchanged.
pub fn resolve_ghost_profile(ghost: &mut crate::config::GhostConfig, profiles: &crate::config::ToolProfiles) {
    let profile_name = match ghost.profile.as_ref() {
        Some(p) => p.clone(),
        None => return,
    };
    let profile_tools = match profiles.get(&profile_name) {
        Some(t) => t,
        None => {
            tracing::warn!(
                ghost = %ghost.name,
                profile = %profile_name,
                "Ghost references unknown tool profile — ignoring"
            );
            return;
        }
    };
    // Append profile tools not already in the inline list
    let existing: std::collections::HashSet<_> = ghost.tools.iter().cloned().collect();
    for tool in profile_tools {
        if !existing.contains(tool) {
            ghost.tools.push(tool.clone());
        }
    }
}
```

- [ ] **Step 4: Call `resolve_ghost_profile` during ghost loading**

Find the function in `profiles.rs` that produces the final `Vec<GhostConfig>` (likely `load_ghosts()` or a helper). After each `GhostConfig` is constructed and before it's pushed to the result, add:

```rust
resolve_ghost_profile(&mut ghost, &config.tool_profiles);
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p sparks profile_resolution_tests 2>&1
```

- [ ] **Step 6: Compile check**

```bash
cargo check 2>&1 | head -20
```

- [ ] **Step 7: Commit**

```bash
git add src/profiles.rs
git commit -m "feat(profiles): resolve tool profile references during ghost loading"
```

---

## Task 3: Validate profiles in `doctor.rs`

**Files:**
- Modify: `src/doctor.rs`
- Read first: `src/doctor.rs` to understand how existing checks are structured

- [ ] **Step 1: Read `doctor.rs` to understand check structure**

```bash
grep -n "fn check\|pub fn\|warn\|error" src/doctor.rs | head -30
```

- [ ] **Step 2: Write failing test**

Add to `src/doctor.rs`:

```rust
#[cfg(test)]
mod profile_validation_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn validate_profiles_finds_missing_profile_reference() {
        let mut profiles: crate::config::ToolProfiles = HashMap::new();
        profiles.insert("researcher".to_string(), vec!["web_search".to_string()]);

        let issues = validate_tool_profiles(
            &profiles,
            &["nonexistent_profile".to_string()], // ghost references this
            &["web_search".to_string()],           // registered tools
        );
        assert!(issues.iter().any(|i| i.contains("nonexistent_profile")));
    }

    #[test]
    fn validate_profiles_finds_unregistered_tool_in_profile() {
        let mut profiles: crate::config::ToolProfiles = HashMap::new();
        profiles.insert("test".to_string(), vec!["phantom_tool".to_string()]);

        let issues = validate_tool_profiles(
            &profiles,
            &["test".to_string()],
            &["web_search".to_string()], // phantom_tool not registered
        );
        assert!(issues.iter().any(|i| i.contains("phantom_tool")));
    }

    #[test]
    fn validate_profiles_no_issues_when_valid() {
        let mut profiles: crate::config::ToolProfiles = HashMap::new();
        profiles.insert("researcher".to_string(), vec!["web_search".to_string()]);

        let issues = validate_tool_profiles(
            &profiles,
            &["researcher".to_string()],
            &["web_search".to_string()],
        );
        assert!(issues.is_empty());
    }
}
```

- [ ] **Step 3: Run test — expect compile failure**

```bash
cargo test -p sparks profile_validation_tests 2>&1 | head -20
```

- [ ] **Step 4: Add `validate_tool_profiles` function**

Add to `src/doctor.rs`:

```rust
/// Validate tool profile references and tool registration.
/// Returns a list of warning strings (not errors — startup continues).
pub fn validate_tool_profiles(
    profiles: &crate::config::ToolProfiles,
    referenced_profiles: &[String],
    registered_tools: &[String],
) -> Vec<String> {
    let registered: std::collections::HashSet<_> = registered_tools.iter().collect();
    let mut issues = Vec::new();

    for profile_ref in referenced_profiles {
        if !profiles.contains_key(profile_ref) {
            issues.push(format!(
                "Ghost references unknown tool profile '{}' — check [tool_profiles] in config",
                profile_ref
            ));
        }
    }

    for (profile_name, tools) in profiles {
        for tool in tools {
            if !registered.contains(tool) {
                issues.push(format!(
                    "Tool profile '{}' references unregistered tool '{}' — may be an MCP tool not yet connected",
                    profile_name, tool
                ));
            }
        }
    }

    issues
}
```

- [ ] **Step 5: Wire into the existing `doctor` command**

Find where `doctor` runs its checks (likely a `run_checks()` or similar function). Add a call to `validate_tool_profiles` and log each issue as a warning:

```rust
let profile_refs: Vec<String> = config.ghosts
    .iter()
    .filter_map(|g| g.profile.clone())
    .collect();
let registered_tools: Vec<String> = /* collect from ToolRegistry or config */;

for issue in crate::doctor::validate_tool_profiles(&config.tool_profiles, &profile_refs, &registered_tools) {
    tracing::warn!("[doctor] {}", issue);
}
```

Note: For `registered_tools`, use whatever the doctor command already uses to enumerate available tools. If it's not available, use an empty slice for now — the check will silently pass for tool existence.

- [ ] **Step 6: Run tests**

```bash
cargo test -p sparks profile_validation_tests 2>&1
```

- [ ] **Step 7: Run full test suite**

```bash
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 8: Commit**

```bash
git add src/doctor.rs
git commit -m "feat(profiles): validate tool profile references and tool registration in doctor"
```

---

## Task 4: Update `config.example.toml`

**Files:**
- Modify: `config.example.toml`

- [ ] **Step 1: Add `[tool_profiles]` section**

Find a good location in `config.example.toml` (near the `[[ghosts]]` section) and add:

```toml
# Named tool profiles — reference from a ghost with profile = "name"
# All tools in a profile must be registered (built-in or MCP).
# Inline [[ghosts]] tools list takes precedence over profile tools.
#
# [tool_profiles]
# researcher = ["web_search", "file_read", "grep", "memory_search"]
# devops     = ["shell", "git", "gh", "docker_exec"]
# reviewer   = ["file_read", "grep", "glob", "git"]
```

Also add a commented example to the `[[ghosts]]` example:

```toml
# profile = "researcher"   # use a named tool profile instead of or alongside tools = [...]
```

- [ ] **Step 2: Compile + final test run**

```bash
cargo check 2>&1 | head -10
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 3: Commit**

```bash
git add config.example.toml
git commit -m "docs: add tool_profiles example to config.example.toml"
```
