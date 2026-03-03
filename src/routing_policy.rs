use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Coding,
    Analysis,
    Mixed,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutingSignals {
    pub requested_ghost: Option<String>,
    pub risk_tier: String,
    pub task_type: TaskType,
    pub success_rate: Option<f64>,
    pub verification_pass_rate: Option<f64>,
    pub rollback_rate: Option<f64>,
    pub token_confidence: f64,
    pub tool_failure_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutingDecision {
    pub route_kind: String,
    pub selected_ghost: String,
    pub selected_strategy: String,
    pub selected_score: f64,
    pub coder_score: f64,
    pub scout_score: f64,
    pub rationale: Vec<String>,
    pub conservative: bool,
}

pub fn infer_task_type(goal: &str) -> TaskType {
    let g = goal.to_lowercase();
    let coding = [
        "implement",
        "fix",
        "refactor",
        "edit",
        "write code",
        "compile",
        "cargo",
        "test",
        "build",
    ]
    .iter()
    .any(|k| g.contains(k));
    let analysis = [
        "analyze",
        "explore",
        "inspect",
        "review",
        "investigate",
        "read",
        "summarize",
    ]
    .iter()
    .any(|k| g.contains(k));

    match (coding, analysis) {
        (true, false) => TaskType::Coding,
        (false, true) => TaskType::Analysis,
        _ => TaskType::Mixed,
    }
}

fn select_strategy_for_ghost(ghost: &str) -> &'static str {
    match ghost {
        "coder" => "code",
        _ => "react",
    }
}

pub fn decide_route(signals: &RoutingSignals, available_ghosts: &[String]) -> RoutingDecision {
    let has_coder = available_ghosts.iter().any(|g| g == "coder");
    let has_scout = available_ghosts.iter().any(|g| g == "scout");

    if let Some(explicit) = &signals.requested_ghost {
        let ghost = if available_ghosts.iter().any(|g| g == explicit) {
            explicit.clone()
        } else if has_coder {
            "coder".to_string()
        } else if has_scout {
            "scout".to_string()
        } else {
            available_ghosts
                .first()
                .cloned()
                .unwrap_or_else(|| "scout".to_string())
        };
        return RoutingDecision {
            route_kind: "explicit_ghost".to_string(),
            selected_strategy: select_strategy_for_ghost(&ghost).to_string(),
            selected_score: 1.0,
            selected_ghost: ghost,
            coder_score: 0.0,
            scout_score: 0.0,
            rationale: vec!["honored explicit ghost selection".to_string()],
            conservative: false,
        };
    }

    let mut coder_score = 0.0;
    let mut scout_score = 0.0;
    let mut rationale = Vec::new();

    match signals.task_type {
        TaskType::Coding => {
            coder_score += 3.0;
            rationale.push("coding task signal -> prefer coder".to_string());
        }
        TaskType::Analysis => {
            scout_score += 3.0;
            rationale.push("analysis task signal -> prefer scout".to_string());
        }
        TaskType::Mixed => {
            coder_score += 1.0;
            scout_score += 1.0;
            rationale.push("mixed task signal -> neutral start".to_string());
        }
    }

    match signals.risk_tier.as_str() {
        "high" | "critical" => {
            scout_score += 2.0;
            rationale.push("high risk -> conservative routing".to_string());
        }
        "low" => {
            coder_score += 0.5;
            rationale.push("low risk -> allow aggressive coding path".to_string());
        }
        _ => {}
    }

    if let Some(success_rate) = signals.success_rate {
        if success_rate < 0.5 {
            scout_score += 1.5;
            rationale.push("low recent success rate -> prefer safer route".to_string());
        } else if success_rate > 0.75 {
            coder_score += 0.8;
            rationale.push("strong recent success rate -> prefer coder".to_string());
        }
    }

    if let Some(verification_pass_rate) = signals.verification_pass_rate {
        if verification_pass_rate < 0.6 {
            scout_score += 1.0;
            rationale.push("weak verification pass rate -> safer route".to_string());
        }
    }

    if let Some(rollback_rate) = signals.rollback_rate {
        if rollback_rate > 0.2 {
            scout_score += 1.2;
            rationale.push("high rollback rate -> conservative route".to_string());
        }
    }

    if signals.token_confidence < 0.5 {
        scout_score += 0.8;
        rationale.push("low token-confidence estimate -> reduce complexity".to_string());
    }

    if signals.tool_failure_rate > 0.35 {
        scout_score += 0.8;
        rationale.push("elevated tool failure rate -> conservative route".to_string());
    }

    let mut selected = if coder_score >= scout_score {
        "coder".to_string()
    } else {
        "scout".to_string()
    };

    if !available_ghosts.iter().any(|g| g == &selected) {
        if has_coder {
            selected = "coder".to_string();
        } else if has_scout {
            selected = "scout".to_string();
        } else {
            selected = available_ghosts
                .first()
                .cloned()
                .unwrap_or_else(|| "scout".to_string());
        }
        rationale.push("selected ghost unavailable -> fallback applied".to_string());
    }

    let selected_score = if selected == "coder" {
        coder_score
    } else {
        scout_score
    };
    let conservative = scout_score > coder_score;
    let route_kind = if conservative {
        "conservative_weighted".to_string()
    } else {
        "balanced_weighted".to_string()
    };

    RoutingDecision {
        route_kind,
        selected_ghost: selected.clone(),
        selected_strategy: select_strategy_for_ghost(&selected).to_string(),
        selected_score,
        coder_score,
        scout_score,
        rationale,
        conservative,
    }
}

#[cfg(test)]
mod tests {
    use super::{decide_route, infer_task_type, RoutingSignals, TaskType};

    #[test]
    fn explicit_ghost_wins() {
        let signals = RoutingSignals {
            requested_ghost: Some("coder".to_string()),
            risk_tier: "high".to_string(),
            task_type: TaskType::Analysis,
            success_rate: Some(0.2),
            verification_pass_rate: Some(0.3),
            rollback_rate: Some(0.4),
            token_confidence: 0.2,
            tool_failure_rate: 0.6,
        };
        let d = decide_route(&signals, &["coder".to_string(), "scout".to_string()]);
        assert_eq!(d.selected_ghost, "coder");
        assert_eq!(d.route_kind, "explicit_ghost");
    }

    #[test]
    fn conservative_route_for_risky_weak_signals() {
        let signals = RoutingSignals {
            requested_ghost: None,
            risk_tier: "high".to_string(),
            task_type: TaskType::Coding,
            success_rate: Some(0.3),
            verification_pass_rate: Some(0.2),
            rollback_rate: Some(0.5),
            token_confidence: 0.4,
            tool_failure_rate: 0.7,
        };
        let d = decide_route(&signals, &["coder".to_string(), "scout".to_string()]);
        assert_eq!(d.selected_ghost, "scout");
        assert!(d.conservative);
    }

    #[test]
    fn coding_task_prefers_coder_when_healthy() {
        let signals = RoutingSignals {
            requested_ghost: None,
            risk_tier: "low".to_string(),
            task_type: TaskType::Coding,
            success_rate: Some(0.9),
            verification_pass_rate: Some(0.95),
            rollback_rate: Some(0.01),
            token_confidence: 0.9,
            tool_failure_rate: 0.05,
        };
        let d = decide_route(&signals, &["coder".to_string(), "scout".to_string()]);
        assert_eq!(d.selected_ghost, "coder");
    }

    #[test]
    fn infer_task_type_detects_mixed_queries() {
        let ty = infer_task_type("analyze code paths and then implement fix");
        assert_eq!(ty, TaskType::Mixed);
    }

    #[test]
    fn fallback_when_selected_ghost_is_missing() {
        let signals = RoutingSignals {
            requested_ghost: None,
            risk_tier: "high".to_string(),
            task_type: TaskType::Analysis,
            success_rate: Some(0.2),
            verification_pass_rate: Some(0.2),
            rollback_rate: Some(0.5),
            token_confidence: 0.2,
            tool_failure_rate: 0.7,
        };
        let d = decide_route(&signals, &["coder".to_string()]);
        assert_eq!(d.selected_ghost, "coder");
    }
}
