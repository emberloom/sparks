use anyhow::Context;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

const ALLOWED_CLI_TOOLS: &[&str] = &["claude_code", "codex", "opencode"];
const ALLOWED_VERIFY_PROFILES: &[&str] = &["fast", "strict"];

#[derive(Debug, Clone, Deserialize)]
pub struct FeatureContract {
    pub feature_id: String,
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub verification_checks: Vec<VerificationCheck>,
    #[serde(default)]
    pub tasks: Vec<FeatureTask>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    // Set from YAML/JSON contract; not currently read by Rust code
    #[allow(dead_code, reason = "retained for serde/db compatibility")]
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VerificationCheck {
    pub id: String,
    pub command: String,
    #[serde(default = "default_verify_profile")]
    pub profile: String,
    #[serde(default)]
    pub mapped_acceptance: Vec<String>,
    #[serde(default = "default_required")]
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeatureTask {
    pub id: String,
    pub goal: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub ghost: Option<String>,
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub auto_store: Option<String>,
    #[serde(default)]
    pub wait_secs: Option<u64>,
    #[serde(default)]
    pub cli_tool: Option<String>,
    #[serde(default)]
    pub cli_model: Option<String>,
    #[serde(default)]
    pub mapped_acceptance: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

fn default_required() -> bool {
    true
}

fn default_verify_profile() -> String {
    "strict".to_string()
}

const FEATURE_TEMPLATE_LINEAR: &str = r#"feature_id = "__FEATURE_ID__"
lane = "delivery"
risk = "medium"
repo = "sparks"

[[acceptance_criteria]]
id = "AC-1"
description = "Planning and implementation complete."

[[acceptance_criteria]]
id = "AC-2"
description = "Regression coverage added and passing."

[[acceptance_criteria]]
id = "AC-3"
description = "Operator docs updated."

[[verification_checks]]
id = "VC-fast"
profile = "fast"
command = "cargo check --quiet"
mapped_acceptance = ["AC-1"]
required = true

[[verification_checks]]
id = "VC-strict"
profile = "strict"
command = "cargo test --quiet"
mapped_acceptance = ["AC-2", "AC-3"]
required = true

[[tasks]]
id = "T1"
goal = "Implement the core change"
mapped_acceptance = ["AC-1"]

[[tasks]]
id = "T2"
depends_on = ["T1"]
goal = "Add tests for the new behavior"
mapped_acceptance = ["AC-2"]

[[tasks]]
id = "T3"
depends_on = ["T2"]
goal = "Update documentation and examples"
mapped_acceptance = ["AC-3"]
"#;

const FEATURE_TEMPLATE_FANOUT_FANIN: &str = r#"feature_id = "__FEATURE_ID__"
lane = "delivery"
risk = "medium"
repo = "sparks"

[[acceptance_criteria]]
id = "AC-1"
description = "Shared foundation task is complete."

[[acceptance_criteria]]
id = "AC-2"
description = "Parallel implementation branch A is complete."

[[acceptance_criteria]]
id = "AC-3"
description = "Parallel implementation branch B is complete."

[[acceptance_criteria]]
id = "AC-4"
description = "Integration and final verification complete."

[[verification_checks]]
id = "VC-fast"
profile = "fast"
command = "cargo check --quiet"
mapped_acceptance = ["AC-1", "AC-2", "AC-3"]
required = true

[[verification_checks]]
id = "VC-strict"
profile = "strict"
command = "cargo test --quiet"
mapped_acceptance = ["AC-4"]
required = true

[[tasks]]
id = "T1"
goal = "Prepare shared scaffolding and baseline plumbing"
mapped_acceptance = ["AC-1"]

[[tasks]]
id = "T2"
depends_on = ["T1"]
goal = "Implement branch A changes"
mapped_acceptance = ["AC-2"]

[[tasks]]
id = "T3"
depends_on = ["T1"]
goal = "Implement branch B changes"
mapped_acceptance = ["AC-3"]

[[tasks]]
id = "T4"
depends_on = ["T2", "T3"]
goal = "Integrate branches and finish acceptance evidence"
mapped_acceptance = ["AC-4"]
"#;

pub fn render_feature_contract_template_toml(
    feature_id: &str,
    pattern: &str,
) -> anyhow::Result<String> {
    let feature_id = normalized_feature_id(feature_id);
    let template = match pattern {
        "linear" => FEATURE_TEMPLATE_LINEAR,
        "fanout-fanin" => FEATURE_TEMPLATE_FANOUT_FANIN,
        _ => {
            anyhow::bail!(
                "Unsupported template pattern '{}'. Use: linear | fanout-fanin",
                pattern
            )
        }
    };
    Ok(template.replace("__FEATURE_ID__", feature_id))
}

fn normalized_feature_id(feature_id: &str) -> &str {
    let trimmed = feature_id.trim();
    if trimmed.is_empty() {
        "my-feature-id"
    } else {
        trimmed
    }
}

pub fn load_feature_contract(path: &Path) -> anyhow::Result<FeatureContract> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("Failed to read feature contract file {}", path.display()))?;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut parsed: FeatureContract = match ext.as_str() {
        "json" => serde_json::from_str(&raw).context("Failed to parse JSON feature contract"),
        "yml" | "yaml" => {
            serde_yaml::from_str(&raw).context("Failed to parse YAML feature contract")
        }
        "toml" => toml::from_str(&raw).context("Failed to parse TOML feature contract"),
        _ => {
            let yaml_err = serde_yaml::from_str::<FeatureContract>(&raw).err();
            let json_err = serde_json::from_str::<FeatureContract>(&raw).err();
            let toml_err = toml::from_str::<FeatureContract>(&raw).err();
            match (yaml_err, json_err, toml_err) {
                (None, _, _) => serde_yaml::from_str(&raw)
                    .context("Failed to parse feature contract as YAML (unexpected parse error)"),
                (_, None, _) => serde_json::from_str(&raw)
                    .context("Failed to parse feature contract as JSON (unexpected parse error)"),
                (_, _, None) => toml::from_str(&raw)
                    .context("Failed to parse feature contract as TOML (unexpected parse error)"),
                (Some(y), Some(j), Some(t)) => anyhow::bail!(
                    "Failed to parse feature contract '{}'. Supported formats: YAML, JSON, TOML.\nYAML error: {}\nJSON error: {}\nTOML error: {}",
                    path.display(),
                    y,
                    j,
                    t
                ),
            }
        }
    }?;
    parsed.validate()?;
    Ok(parsed)
}

impl FeatureContract {
    pub fn validate(&mut self) -> anyhow::Result<()> {
        let mut errors: Vec<String> = Vec::new();
        if self.feature_id.trim().is_empty() {
            errors.push("feature_id must not be empty".to_string());
        }
        normalize_lane_and_risk(
            self.lane.as_deref(),
            self.risk.as_deref(),
            "contract defaults",
            &mut errors,
        );
        if self.tasks.is_empty() {
            errors.push("tasks must not be empty".to_string());
        }
        if self.acceptance_criteria.is_empty() {
            errors.push("acceptance_criteria must not be empty".to_string());
        }

        let mut acceptance_ids = BTreeSet::new();
        for (idx, ac) in self.acceptance_criteria.iter().enumerate() {
            if ac.id.trim().is_empty() {
                errors.push(format!("acceptance_criteria[{}].id must not be empty", idx));
            } else if !acceptance_ids.insert(ac.id.clone()) {
                errors.push(format!(
                    "acceptance_criteria[{}].id '{}' is duplicated",
                    idx, ac.id
                ));
            }
        }
        let mut acceptance_coverage: BTreeMap<String, usize> = BTreeMap::new();
        for id in &acceptance_ids {
            acceptance_coverage.insert(id.clone(), 0);
        }

        let mut verification_ids = BTreeSet::new();
        for (idx, check) in self.verification_checks.iter().enumerate() {
            if check.id.trim().is_empty() {
                errors.push(format!("verification_checks[{}].id must not be empty", idx));
            } else if !verification_ids.insert(check.id.clone()) {
                errors.push(format!(
                    "verification_checks[{}].id '{}' is duplicated",
                    idx, check.id
                ));
            }
            if check.command.trim().is_empty() {
                errors.push(format!(
                    "verification_checks[{}] '{}' has empty command",
                    idx, check.id
                ));
            }
            if !ALLOWED_VERIFY_PROFILES.contains(&check.profile.as_str()) {
                errors.push(format!(
                    "verification_checks[{}] '{}' has invalid profile '{}' (expected one of: {})",
                    idx,
                    check.id,
                    check.profile,
                    ALLOWED_VERIFY_PROFILES.join(", ")
                ));
            }
            if check.mapped_acceptance.is_empty() {
                errors.push(format!(
                    "verification_checks[{}] '{}' must map at least one acceptance criterion",
                    idx, check.id
                ));
            }
            for (mapped_idx, mapped) in check.mapped_acceptance.iter().enumerate() {
                if !acceptance_ids.contains(mapped) {
                    let suggestion = suggest_similar_id(mapped, acceptance_ids.iter());
                    errors.push(format!(
                        "verification_checks[{}].mapped_acceptance[{}] references unknown acceptance criterion '{}'{}",
                        idx,
                        mapped_idx,
                        mapped,
                        suggestion
                            .as_deref()
                            .map(|id| format!(" (did you mean '{}'?)", id))
                            .unwrap_or_default()
                    ));
                }
            }
        }

        let mut task_ids = BTreeSet::new();
        for (idx, task) in self.tasks.iter().enumerate() {
            let label = format!("tasks[{}] '{}'", idx, task.id);
            if task.id.trim().is_empty() {
                errors.push(format!("tasks[{}].id must not be empty", idx));
            } else if !task_ids.insert(task.id.clone()) {
                errors.push(format!("tasks[{}].id '{}' is duplicated", idx, task.id));
            }
            if task.enabled && task.goal.trim().is_empty() {
                errors.push(format!("{} has empty goal", label));
            }
            if task.enabled && task.mapped_acceptance.is_empty() {
                errors.push(format!(
                    "{} must map at least one acceptance criterion",
                    label
                ));
            }
            normalize_lane_and_risk(
                task.lane.as_deref(),
                task.risk.as_deref(),
                &label,
                &mut errors,
            );
            if let Some(tool) = task.cli_tool.as_deref() {
                if !ALLOWED_CLI_TOOLS.contains(&tool) {
                    errors.push(format!(
                        "{} has unsupported cli_tool '{}' (expected one of: {})",
                        label,
                        tool,
                        ALLOWED_CLI_TOOLS.join(", ")
                    ));
                }
            }
            for dep in &task.depends_on {
                if dep == &task.id {
                    errors.push(format!("{} depends on itself", label));
                }
            }
            if task.enabled {
                for (mapped_idx, mapped) in task.mapped_acceptance.iter().enumerate() {
                    if !acceptance_ids.contains(mapped) {
                        let suggestion = suggest_similar_id(mapped, acceptance_ids.iter());
                        errors.push(format!(
                            "{} maps unknown acceptance criterion '{}' at mapped_acceptance[{}]{}",
                            label,
                            mapped,
                            mapped_idx,
                            suggestion
                                .as_deref()
                                .map(|id| format!(" (did you mean '{}'?)", id))
                                .unwrap_or_default()
                        ));
                    } else if let Some(count) = acceptance_coverage.get_mut(mapped) {
                        *count += 1;
                    }
                }
            }
        }

        let task_by_id: HashMap<String, &FeatureTask> =
            self.tasks.iter().map(|t| (t.id.clone(), t)).collect();
        for (task_idx, task) in self.tasks.iter().enumerate() {
            if !task.enabled {
                continue;
            }
            for (dep_idx, dep) in task.depends_on.iter().enumerate() {
                match task_by_id.get(dep) {
                    None => {
                        let suggestion = suggest_similar_id(dep, task_ids.iter());
                        errors.push(format!(
                            "tasks[{}].depends_on[{}] references unknown task '{}'{}",
                            task_idx,
                            dep_idx,
                            dep,
                            suggestion
                                .as_deref()
                                .map(|id| format!(" (did you mean '{}'?)", id))
                                .unwrap_or_default()
                        ));
                    }
                    Some(dep_task) if !dep_task.enabled => errors.push(format!(
                        "tasks[{}].depends_on[{}] references disabled task '{}'",
                        task_idx, dep_idx, dep
                    )),
                    _ => {}
                }
            }
        }

        if errors.is_empty() {
            if let Err(e) = self.execution_batches() {
                errors.push(e.to_string());
            }
        }
        if errors.is_empty() {
            for (id, count) in acceptance_coverage {
                if count == 0 {
                    errors.push(format!(
                        "acceptance criterion '{}' is not covered by any enabled task",
                        id
                    ));
                }
            }
        }

        if !errors.is_empty() {
            errors.sort();
            errors.dedup();
            anyhow::bail!(
                "Invalid feature contract ({}):\n- {}",
                if self.feature_id.trim().is_empty() {
                    "<empty>"
                } else {
                    &self.feature_id
                },
                errors.join("\n- ")
            );
        }
        Ok(())
    }

    pub fn task_by_id(&self, id: &str) -> Option<&FeatureTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    #[cfg(test)]
    pub fn execution_order(&self) -> anyhow::Result<Vec<String>> {
        let batches = self.execution_batches()?;
        Ok(batches.into_iter().flatten().collect())
    }

    pub fn execution_batches(&self) -> anyhow::Result<Vec<Vec<String>>> {
        let enabled: Vec<&FeatureTask> = self.tasks.iter().filter(|t| t.enabled).collect();
        if enabled.is_empty() {
            return Ok(Vec::new());
        }

        let (mut indegree, adjacency, depends_on_enabled) =
            build_enabled_dependency_graph(&enabled)?;

        let mut ready: BTreeSet<String> = indegree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| id.clone())
            .collect();
        let mut processed = 0usize;
        let mut batches: Vec<Vec<String>> = Vec::new();

        while !ready.is_empty() {
            let current: Vec<String> = ready.iter().cloned().collect();
            ready.clear();
            for node in &current {
                processed += 1;
                if let Some(children) = adjacency.get(node) {
                    for child in children {
                        if let Some(degree) = indegree.get_mut(child) {
                            *degree = degree.saturating_sub(1);
                            if *degree == 0 {
                                ready.insert(child.clone());
                            }
                        }
                    }
                }
            }
            batches.push(current);
        }

        if processed != enabled.len() {
            let remaining: BTreeSet<String> = indegree
                .iter()
                .filter_map(|(id, degree)| (*degree > 0).then_some(id.clone()))
                .collect();
            anyhow::bail!("{}", format_cycle_error(&remaining, &depends_on_enabled));
        }

        Ok(batches)
    }

    pub fn acceptance_coverage(&self) -> BTreeMap<String, Vec<String>> {
        let mut coverage: BTreeMap<String, Vec<String>> = self
            .acceptance_criteria
            .iter()
            .map(|a| (a.id.clone(), Vec::new()))
            .collect();
        for task in self.tasks.iter().filter(|t| t.enabled) {
            for mapped in &task.mapped_acceptance {
                coverage
                    .entry(mapped.clone())
                    .or_default()
                    .push(task.id.clone());
            }
        }
        for tasks in coverage.values_mut() {
            tasks.sort();
            tasks.dedup();
        }
        coverage
    }

    pub fn acceptance_verification_coverage(&self) -> BTreeMap<String, Vec<String>> {
        let mut coverage: BTreeMap<String, Vec<String>> = self
            .acceptance_criteria
            .iter()
            .map(|a| (a.id.clone(), Vec::new()))
            .collect();
        for check in &self.verification_checks {
            for mapped in &check.mapped_acceptance {
                coverage
                    .entry(mapped.clone())
                    .or_default()
                    .push(check.id.clone());
            }
        }
        for checks in coverage.values_mut() {
            checks.sort();
            checks.dedup();
        }
        coverage
    }
}

fn normalize_lane_and_risk(
    lane: Option<&str>,
    risk: Option<&str>,
    label: &str,
    errors: &mut Vec<String>,
) {
    if let Some(l) = lane {
        if !matches!(l, "delivery" | "self_improvement") {
            errors.push(format!(
                "{} has invalid lane '{}' (expected delivery | self_improvement)",
                label, l
            ));
        }
    }
    if let Some(r) = risk {
        if !matches!(r, "low" | "medium" | "high") {
            errors.push(format!(
                "{} has invalid risk '{}' (expected low | medium | high)",
                label, r
            ));
        }
    }
}

fn build_enabled_dependency_graph(
    enabled: &[&FeatureTask],
) -> anyhow::Result<(
    HashMap<String, usize>,
    HashMap<String, Vec<String>>,
    HashMap<String, Vec<String>>,
)> {
    let mut indegree: HashMap<String, usize> = HashMap::new();
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    let mut depends_on_enabled: HashMap<String, Vec<String>> = HashMap::new();
    for task in enabled {
        indegree.insert(task.id.clone(), 0);
        adjacency.entry(task.id.clone()).or_default();
        depends_on_enabled.insert(task.id.clone(), Vec::new());
    }
    for task in enabled {
        for dep in &task.depends_on {
            if !indegree.contains_key(dep) {
                continue;
            }
            let Some(degree) = indegree.get_mut(&task.id) else {
                anyhow::bail!(
                    "internal error: enabled node '{}' missing indegree",
                    task.id
                );
            };
            *degree += 1;
            adjacency
                .entry(dep.clone())
                .or_default()
                .push(task.id.clone());
            depends_on_enabled
                .entry(task.id.clone())
                .or_default()
                .push(dep.clone());
        }
    }
    for children in adjacency.values_mut() {
        children.sort();
        children.dedup();
    }
    for deps in depends_on_enabled.values_mut() {
        deps.sort();
        deps.dedup();
    }
    Ok((indegree, adjacency, depends_on_enabled))
}

fn format_cycle_error(
    remaining: &BTreeSet<String>,
    depends_on_enabled: &HashMap<String, Vec<String>>,
) -> String {
    let cycle = find_cycle_trace(remaining, depends_on_enabled)
        .map(|trace| trace.join(" -> "))
        .unwrap_or_else(|| "unable to resolve cycle trace".to_string());
    format!(
        "task dependency graph contains a cycle among enabled tasks: {}. remaining_blocked_tasks={}",
        cycle,
        remaining.iter().cloned().collect::<Vec<_>>().join(",")
    )
}

fn suggest_similar_id<'a>(
    unknown: &str,
    known_ids: impl Iterator<Item = &'a String>,
) -> Option<String> {
    let unknown_lower = unknown.to_ascii_lowercase();
    let mut candidates = known_ids
        .filter(|id| !id.is_empty())
        .map(|id| {
            let id_lower = id.to_ascii_lowercase();
            let prefix_match =
                id_lower.starts_with(&unknown_lower) || unknown_lower.starts_with(&id_lower);
            let distance = edit_distance(&unknown_lower, &id_lower);
            (prefix_match, distance, id.clone())
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let (prefix_match, distance, candidate) = candidates.into_iter().next()?;
    if prefix_match || distance <= 3 {
        Some(candidate)
    } else {
        None
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a_chars = a.chars().collect::<Vec<_>>();
    let b_chars = b.chars().collect::<Vec<_>>();
    let mut prev = (0..=b_chars.len()).collect::<Vec<_>>();
    let mut curr = vec![0usize; b_chars.len() + 1];
    for (i, &ac) in a_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &bc) in b_chars.iter().enumerate() {
            let cost = usize::from(ac != bc);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

fn find_cycle_trace(
    remaining: &BTreeSet<String>,
    depends_on_enabled: &HashMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    let mut visit_state: HashMap<String, u8> = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    for node in remaining {
        if visit_state.get(node).copied().unwrap_or(0) != 0 {
            continue;
        }
        if let Some(trace) = dfs_cycle_trace(
            node,
            remaining,
            depends_on_enabled,
            &mut visit_state,
            &mut stack,
        ) {
            return Some(trace);
        }
    }
    None
}

fn dfs_cycle_trace(
    node: &str,
    remaining: &BTreeSet<String>,
    depends_on_enabled: &HashMap<String, Vec<String>>,
    visit_state: &mut HashMap<String, u8>,
    stack: &mut Vec<String>,
) -> Option<Vec<String>> {
    visit_state.insert(node.to_string(), 1);
    stack.push(node.to_string());
    let mut deps = depends_on_enabled.get(node).cloned().unwrap_or_default();
    deps.sort();
    deps.dedup();
    for dep in deps {
        if !remaining.contains(&dep) {
            continue;
        }
        match visit_state.get(&dep).copied().unwrap_or(0) {
            0 => {
                if let Some(trace) =
                    dfs_cycle_trace(&dep, remaining, depends_on_enabled, visit_state, stack)
                {
                    return Some(trace);
                }
            }
            1 => {
                if let Some(start) = stack.iter().position(|n| n == &dep) {
                    let mut trace = stack[start..].to_vec();
                    trace.push(dep);
                    return Some(trace);
                }
            }
            _ => {}
        }
    }
    stack.pop();
    visit_state.insert(node.to_string(), 2);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_yaml(yaml: &str) -> FeatureContract {
        let mut c: FeatureContract = serde_yaml::from_str(yaml).unwrap();
        c.validate().unwrap();
        c
    }

    #[test]
    fn dag_batches_are_stable_and_layered() {
        let c = parse_yaml(
            r#"
feature_id: demo
lane: delivery
acceptance_criteria:
  - id: AC-1
  - id: AC-2
  - id: AC-3
verification_checks:
  - id: V1
    command: "echo ok"
    mapped_acceptance: [AC-1, AC-2, AC-3]
tasks:
  - id: T1
    goal: one
    mapped_acceptance: [AC-1]
  - id: T2
    goal: two
    mapped_acceptance: [AC-2]
    depends_on: [T1]
  - id: T3
    goal: three
    mapped_acceptance: [AC-2]
    depends_on: [T1]
  - id: T4
    goal: four
    mapped_acceptance: [AC-3]
    depends_on: [T2, T3]
"#,
        );
        assert_eq!(
            c.execution_batches().unwrap(),
            vec![
                vec!["T1".to_string()],
                vec!["T2".to_string(), "T3".to_string()],
                vec!["T4".to_string()],
            ]
        );
        assert_eq!(
            c.execution_order().unwrap(),
            vec![
                "T1".to_string(),
                "T2".to_string(),
                "T3".to_string(),
                "T4".to_string(),
            ]
        );
    }

    #[test]
    fn detects_cycle() {
        let mut c: FeatureContract = serde_yaml::from_str(
            r#"
feature_id: cyc
acceptance_criteria:
  - id: AC-1
verification_checks:
  - id: V1
    command: "echo ok"
    mapped_acceptance: [AC-1]
tasks:
  - id: A
    goal: a
    mapped_acceptance: [AC-1]
    depends_on: [B]
  - id: B
    goal: b
    mapped_acceptance: [AC-1]
    depends_on: [A]
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("contains a cycle"));
        assert!(err.contains("A -> B -> A"));
    }

    #[test]
    fn rejects_dependency_on_disabled_task() {
        let mut c: FeatureContract = serde_yaml::from_str(
            r#"
feature_id: dep
acceptance_criteria:
  - id: AC-1
verification_checks:
  - id: V1
    command: "echo ok"
    mapped_acceptance: [AC-1]
tasks:
  - id: A
    goal: a
    mapped_acceptance: [AC-1]
    enabled: false
  - id: B
    goal: b
    mapped_acceptance: [AC-1]
    depends_on: [A]
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("references disabled task"));
    }

    #[test]
    fn rejects_unmapped_acceptance() {
        let mut c: FeatureContract = serde_yaml::from_str(
            r#"
feature_id: cover
acceptance_criteria:
  - id: AC-1
  - id: AC-2
verification_checks:
  - id: V1
    command: "echo ok"
    mapped_acceptance: [AC-1]
tasks:
  - id: A
    goal: a
    mapped_acceptance: [AC-1]
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("is not covered by any enabled task"));
    }

    #[test]
    fn rejects_unknown_mapped_acceptance() {
        let mut c: FeatureContract = serde_yaml::from_str(
            r#"
feature_id: map
acceptance_criteria:
  - id: AC-1
verification_checks:
  - id: V1
    command: "echo ok"
    mapped_acceptance: [AC-1]
tasks:
  - id: A
    goal: a
    mapped_acceptance: [AC-2]
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("maps unknown acceptance criterion"));
    }

    #[test]
    fn rejects_unknown_acceptance_in_verification_check() {
        let mut c: FeatureContract = serde_yaml::from_str(
            r#"
feature_id: verify-map
acceptance_criteria:
  - id: AC-1
verification_checks:
  - id: V1
    command: "echo ok"
    mapped_acceptance: [AC-2]
tasks:
  - id: A
    goal: a
    mapped_acceptance: [AC-1]
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("verification_checks[0].mapped_acceptance[0]"));
    }

    #[test]
    fn rejects_invalid_verification_profile() {
        let mut c: FeatureContract = serde_yaml::from_str(
            r#"
feature_id: verify-profile
acceptance_criteria:
  - id: AC-1
verification_checks:
  - id: V1
    command: "echo ok"
    profile: "nightly"
    mapped_acceptance: [AC-1]
tasks:
  - id: A
    goal: a
    mapped_acceptance: [AC-1]
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("invalid profile"));
    }

    #[test]
    fn unknown_dependency_diagnostic_suggests_closest_id() {
        let mut c: FeatureContract = serde_yaml::from_str(
            r#"
feature_id: dep-suggest
acceptance_criteria:
  - id: AC-1
verification_checks:
  - id: V1
    command: "echo ok"
    mapped_acceptance: [AC-1]
tasks:
  - id: TASK-1
    goal: one
    mapped_acceptance: [AC-1]
  - id: TASK-2
    goal: two
    mapped_acceptance: [AC-1]
    depends_on: [TSAK-1]
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("references unknown task 'TSAK-1'"));
        assert!(err.contains("did you mean 'TASK-1'"));
    }

    #[test]
    fn template_generation_outputs_valid_contracts() {
        for pattern in ["linear", "fanout-fanin"] {
            let raw = render_feature_contract_template_toml("demo-template", pattern)
                .expect("template should render");
            let mut c: FeatureContract = toml::from_str(&raw).expect("template should parse");
            c.validate().expect("template should validate");
        }
    }

    #[test]
    fn coverage_ordering_is_deterministic() {
        let c = parse_yaml(
            r#"
feature_id: deterministic-coverage
acceptance_criteria:
  - id: AC-2
  - id: AC-1
verification_checks:
  - id: V2
    command: "echo ok"
    mapped_acceptance: [AC-2]
  - id: V1
    command: "echo ok"
    mapped_acceptance: [AC-2, AC-1]
tasks:
  - id: T2
    goal: two
    mapped_acceptance: [AC-2]
  - id: T1
    goal: one
    mapped_acceptance: [AC-2, AC-1]
"#,
        );
        let coverage = c.acceptance_coverage();
        assert_eq!(coverage["AC-2"], vec!["T1".to_string(), "T2".to_string()]);
        let verify = c.acceptance_verification_coverage();
        assert_eq!(verify["AC-2"], vec!["V1".to_string(), "V2".to_string()]);
    }
}
