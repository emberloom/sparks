use std::collections::HashMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};

const POLICY_VERSION: &str = "ghost_policy_v1";
const SCORE_SUCCESS_WEIGHT: f64 = 0.70;
const SCORE_VERIFICATION_WEIGHT: f64 = 0.20;
const SCORE_ROLLBACK_WEIGHT: f64 = 1.00;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhostPolicyScope {
    pub repo: String,
    pub lane: String,
    pub risk_tier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GhostPolicyThresholds {
    pub min_samples: u64,
    pub confidence_threshold: f64,
    pub rollback_min_samples: u64,
    pub max_allowed_regression: f64,
    pub stability_window: usize,
}

impl GhostPolicyThresholds {
    pub fn normalized(mut self) -> Self {
        self.min_samples = self.min_samples.max(1);
        self.rollback_min_samples = self.rollback_min_samples.max(1);
        self.confidence_threshold = self.confidence_threshold.max(0.0);
        self.max_allowed_regression = self.max_allowed_regression.max(0.0);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GhostPolicyMetrics {
    pub ghost: String,
    pub tasks_started: u64,
    pub tasks_succeeded: u64,
    pub success_rate: f64,
    pub verification_total: u64,
    pub verification_passed: u64,
    pub verification_pass_rate: f64,
    pub rollbacks: u64,
    pub rollback_rate: f64,
}

impl GhostPolicyMetrics {
    pub fn score(&self) -> f64 {
        (self.success_rate * SCORE_SUCCESS_WEIGHT)
            + (self.verification_pass_rate * SCORE_VERIFICATION_WEIGHT)
            - (self.rollback_rate * SCORE_ROLLBACK_WEIGHT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhostPolicyAction {
    KeepDefault,
    Promote { candidate: String },
    Rollback { to_baseline: String },
}

impl GhostPolicyAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::KeepDefault => "keep_default",
            Self::Promote { .. } => "promote",
            Self::Rollback { .. } => "rollback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GhostCandidateStats {
    pub ghost: String,
    pub overall: Option<GhostPolicyMetrics>,
    pub recent: Option<GhostPolicyMetrics>,
    pub overall_score: Option<f64>,
    pub recent_score: Option<f64>,
    pub score_margin_vs_baseline: Option<f64>,
    pub rank: Option<usize>,
    pub eligible: bool,
    pub rejection_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GhostPolicyExplanation {
    pub policy_version: String,
    pub decided_at: String,
    pub scope: GhostPolicyScope,
    pub thresholds: GhostPolicyThresholds,
    pub baseline_ghost: String,
    pub previous_selected_ghost: String,
    pub selected_ghost: String,
    pub action: String,
    pub reason_codes: Vec<String>,
    pub baseline_metrics: Option<GhostPolicyMetrics>,
    pub baseline_score: Option<f64>,
    pub candidate_stats: Vec<GhostCandidateStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GhostPolicyDecision {
    pub action: GhostPolicyAction,
    pub selected_ghost: String,
    pub baseline_ghost: String,
    pub explanation: GhostPolicyExplanation,
}

#[derive(Debug, Clone)]
struct RankedCandidate {
    ghost: String,
    score: f64,
    tasks_started: u64,
}

pub fn evaluate_ghost_policy(
    scope: GhostPolicyScope,
    available_ghosts: &[String],
    baseline_ghost: &str,
    previous_selected_ghost: Option<&str>,
    thresholds: GhostPolicyThresholds,
    overall_metrics: &[GhostPolicyMetrics],
    recent_metrics: &[GhostPolicyMetrics],
) -> GhostPolicyDecision {
    let thresholds = thresholds.normalized();

    let mut candidates = available_ghosts
        .iter()
        .map(|g| g.trim())
        .filter(|g| !g.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();

    let fallback_default = if candidates.is_empty() {
        baseline_ghost.to_string()
    } else if candidates.iter().any(|g| g == baseline_ghost) {
        baseline_ghost.to_string()
    } else if candidates.iter().any(|g| g == "coder") {
        "coder".to_string()
    } else {
        candidates[0].clone()
    };

    let previous_selected = previous_selected_ghost
        .filter(|g| candidates.iter().any(|name| name == g))
        .unwrap_or(&fallback_default)
        .to_string();

    let overall_map = metric_map(overall_metrics, &candidates);
    let recent_map = metric_map(recent_metrics, &candidates);

    let baseline_metrics = overall_map.get(&fallback_default).cloned();
    let baseline_score = baseline_metrics.as_ref().map(GhostPolicyMetrics::score);

    let mut candidate_stats = candidates
        .iter()
        .filter(|ghost| ghost.as_str() != fallback_default)
        .map(|ghost| {
            build_candidate_stats(
                ghost,
                &overall_map,
                &recent_map,
                baseline_score,
                baseline_metrics.as_ref(),
                &thresholds,
            )
        })
        .collect::<Vec<_>>();

    let ranked = rank_candidates(&candidate_stats);
    for (idx, ranked_candidate) in ranked.iter().enumerate() {
        if let Some(stat) = candidate_stats
            .iter_mut()
            .find(|stat| stat.ghost == ranked_candidate.ghost)
        {
            stat.rank = Some(idx + 1);
        }
    }

    let mut reason_codes = Vec::new();
    let mut action = GhostPolicyAction::KeepDefault;
    let mut selected_ghost = fallback_default.clone();

    if candidates.is_empty() {
        reason_codes.push("no_configured_ghosts".to_string());
    } else {
        let baseline_ok = baseline_metrics
            .as_ref()
            .map(|m| m.tasks_started >= thresholds.min_samples)
            .unwrap_or(false);
        if !baseline_ok {
            reason_codes.push("baseline_insufficient_samples".to_string());
        }

        if baseline_ok {
            let Some(baseline_metrics_ref) = baseline_metrics.as_ref() else {
                reason_codes.push("baseline_metrics_missing".to_string());
                return GhostPolicyDecision {
                    action: GhostPolicyAction::KeepDefault,
                    selected_ghost: fallback_default.clone(),
                    baseline_ghost: fallback_default.clone(),
                    explanation: GhostPolicyExplanation {
                        policy_version: POLICY_VERSION.to_string(),
                        decided_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        scope,
                        thresholds,
                        baseline_ghost: fallback_default.clone(),
                        previous_selected_ghost: previous_selected,
                        selected_ghost: fallback_default.clone(),
                        action: GhostPolicyAction::KeepDefault.label().to_string(),
                        reason_codes,
                        baseline_metrics,
                        baseline_score,
                        candidate_stats,
                    },
                };
            };
            if previous_selected != fallback_default {
                match evaluate_active_candidate(
                    &previous_selected,
                    &fallback_default,
                    baseline_metrics_ref,
                    &overall_map,
                    &ranked,
                    &thresholds,
                ) {
                    ActiveCandidateDisposition::Rollback => {
                        action = GhostPolicyAction::Rollback {
                            to_baseline: fallback_default.clone(),
                        };
                        selected_ghost = fallback_default.clone();
                        reason_codes.push("candidate_regression_exceeded".to_string());
                    }
                    ActiveCandidateDisposition::KeepBaseline(reason) => {
                        action = GhostPolicyAction::KeepDefault;
                        selected_ghost = fallback_default.clone();
                        reason_codes.push(reason.to_string());
                    }
                    ActiveCandidateDisposition::Promote(active) => {
                        action = GhostPolicyAction::Promote {
                            candidate: active.clone(),
                        };
                        selected_ghost = active;
                        reason_codes.push("promotion_maintained".to_string());
                    }
                }
            } else if let Some(best) = ranked.first() {
                action = GhostPolicyAction::Promote {
                    candidate: best.ghost.clone(),
                };
                selected_ghost = best.ghost.clone();
                reason_codes.push("promote_superior_candidate".to_string());
            } else {
                reason_codes.push("no_eligible_candidate".to_string());
            }
        }
    }

    if reason_codes.is_empty() {
        reason_codes.push("keep_safe_default".to_string());
    }

    let explanation = GhostPolicyExplanation {
        policy_version: POLICY_VERSION.to_string(),
        decided_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        scope,
        thresholds,
        baseline_ghost: fallback_default.clone(),
        previous_selected_ghost: previous_selected,
        selected_ghost: selected_ghost.clone(),
        action: action.label().to_string(),
        reason_codes,
        baseline_metrics,
        baseline_score,
        candidate_stats,
    };

    GhostPolicyDecision {
        action,
        selected_ghost,
        baseline_ghost: fallback_default,
        explanation,
    }
}

enum ActiveCandidateDisposition {
    Rollback,
    KeepBaseline(&'static str),
    Promote(String),
}

fn evaluate_active_candidate(
    active_candidate: &str,
    baseline_ghost: &str,
    baseline_metrics: &GhostPolicyMetrics,
    overall_map: &HashMap<String, GhostPolicyMetrics>,
    ranked: &[RankedCandidate],
    thresholds: &GhostPolicyThresholds,
) -> ActiveCandidateDisposition {
    let Some(active_metrics) = overall_map.get(active_candidate) else {
        return ActiveCandidateDisposition::KeepBaseline("active_candidate_missing_metrics");
    };

    if active_metrics.tasks_started < thresholds.rollback_min_samples {
        return ActiveCandidateDisposition::KeepBaseline("active_candidate_insufficient_samples");
    }
    if baseline_metrics.tasks_started < thresholds.rollback_min_samples {
        return ActiveCandidateDisposition::KeepBaseline("baseline_insufficient_rollback_samples");
    }

    let baseline_score = baseline_metrics.score();
    let active_score = active_metrics.score();
    if baseline_score - active_score > thresholds.max_allowed_regression {
        return ActiveCandidateDisposition::Rollback;
    }

    if let Some(best) = ranked.first() {
        if best.ghost == active_candidate {
            return ActiveCandidateDisposition::Promote(active_candidate.to_string());
        }
        if let Some(best_metrics) = overall_map.get(&best.ghost) {
            let best_score = best_metrics.score();
            if best_score > active_score + thresholds.confidence_threshold {
                return ActiveCandidateDisposition::Promote(best.ghost.clone());
            }
        }
    }

    if active_candidate != baseline_ghost {
        ActiveCandidateDisposition::Promote(active_candidate.to_string())
    } else {
        ActiveCandidateDisposition::KeepBaseline("active_candidate_equals_baseline")
    }
}

fn rank_candidates(stats: &[GhostCandidateStats]) -> Vec<RankedCandidate> {
    let mut ranked = stats
        .iter()
        .filter(|stat| stat.eligible)
        .filter_map(|stat| {
            let overall = stat.overall.as_ref()?;
            Some(RankedCandidate {
                ghost: stat.ghost.clone(),
                score: overall.score(),
                tasks_started: overall.tasks_started,
            })
        })
        .collect::<Vec<_>>();

    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.tasks_started.cmp(&a.tasks_started))
            .then_with(|| a.ghost.cmp(&b.ghost))
    });
    ranked
}

fn build_candidate_stats(
    ghost: &str,
    overall_map: &HashMap<String, GhostPolicyMetrics>,
    recent_map: &HashMap<String, GhostPolicyMetrics>,
    baseline_score: Option<f64>,
    baseline_metrics: Option<&GhostPolicyMetrics>,
    thresholds: &GhostPolicyThresholds,
) -> GhostCandidateStats {
    let overall = overall_map.get(ghost).cloned();
    let recent = recent_map.get(ghost).cloned();
    let overall_score = overall.as_ref().map(GhostPolicyMetrics::score);
    let recent_score = recent.as_ref().map(GhostPolicyMetrics::score);
    let score_margin_vs_baseline = overall_score.zip(baseline_score).map(|(o, b)| o - b);

    let mut rejection_reasons = Vec::new();

    match overall.as_ref() {
        Some(m) => {
            if m.tasks_started < thresholds.min_samples {
                rejection_reasons.push("insufficient_samples".to_string());
            }
        }
        None => rejection_reasons.push("missing_metrics".to_string()),
    }

    if baseline_metrics.is_none() {
        rejection_reasons.push("missing_baseline_metrics".to_string());
    }

    if let Some(margin) = score_margin_vs_baseline {
        if margin < thresholds.confidence_threshold {
            rejection_reasons.push("below_confidence_margin".to_string());
        }
    }

    if thresholds.stability_window > 0 {
        let required_recent = (thresholds
            .min_samples
            .min(thresholds.stability_window as u64))
        .max(1);
        match (overall.as_ref(), recent.as_ref()) {
            (Some(ov), Some(rec)) => {
                if rec.tasks_started < required_recent {
                    rejection_reasons.push("insufficient_recent_samples".to_string());
                } else if (rec.score() - ov.score()).abs() > thresholds.confidence_threshold {
                    rejection_reasons.push("unstable_recent_performance".to_string());
                }
            }
            _ => rejection_reasons.push("insufficient_recent_samples".to_string()),
        }
    }

    GhostCandidateStats {
        ghost: ghost.to_string(),
        overall,
        recent,
        overall_score,
        recent_score,
        score_margin_vs_baseline,
        rank: None,
        eligible: rejection_reasons.is_empty(),
        rejection_reasons,
    }
}

fn metric_map(
    metrics: &[GhostPolicyMetrics],
    configured_ghosts: &[String],
) -> HashMap<String, GhostPolicyMetrics> {
    metrics
        .iter()
        .filter(|metric| configured_ghosts.iter().any(|g| g == &metric.ghost))
        .cloned()
        .map(|metric| (metric.ghost.clone(), metric))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(
        ghost: &str,
        tasks_started: u64,
        tasks_succeeded: u64,
        verification_total: u64,
        verification_passed: u64,
        rollbacks: u64,
    ) -> GhostPolicyMetrics {
        let success_rate = if tasks_started == 0 {
            0.0
        } else {
            tasks_succeeded as f64 / tasks_started as f64
        };
        let verification_pass_rate = if verification_total == 0 {
            0.0
        } else {
            verification_passed as f64 / verification_total as f64
        };
        let rollback_rate = if tasks_started == 0 {
            0.0
        } else {
            rollbacks as f64 / tasks_started as f64
        };

        GhostPolicyMetrics {
            ghost: ghost.to_string(),
            tasks_started,
            tasks_succeeded,
            success_rate,
            verification_total,
            verification_passed,
            verification_pass_rate,
            rollbacks,
            rollback_rate,
        }
    }

    fn scope() -> GhostPolicyScope {
        GhostPolicyScope {
            repo: "sparks".to_string(),
            lane: "ticket_intake".to_string(),
            risk_tier: Some("medium".to_string()),
        }
    }

    fn thresholds() -> GhostPolicyThresholds {
        GhostPolicyThresholds {
            min_samples: 3,
            confidence_threshold: 0.05,
            rollback_min_samples: 3,
            max_allowed_regression: 0.08,
            stability_window: 0,
        }
    }

    #[test]
    fn deterministic_tie_break_prefers_lexicographic_ghost_name() {
        let decision = evaluate_ghost_policy(
            scope(),
            &["coder".into(), "architect".into(), "scout".into()],
            "coder",
            None,
            thresholds(),
            &[
                metrics("coder", 4, 1, 4, 2, 0),
                metrics("architect", 4, 3, 4, 4, 1),
                metrics("scout", 4, 3, 4, 4, 1),
            ],
            &[],
        );

        assert_eq!(
            decision.action,
            GhostPolicyAction::Promote {
                candidate: "architect".to_string()
            }
        );
        assert_eq!(decision.selected_ghost, "architect");
    }

    #[test]
    fn promotion_requires_sample_threshold() {
        let decision = evaluate_ghost_policy(
            scope(),
            &["coder".into(), "scout".into()],
            "coder",
            None,
            thresholds(),
            &[
                metrics("coder", 5, 3, 5, 4, 0),
                metrics("scout", 2, 2, 2, 2, 0),
            ],
            &[],
        );

        assert_eq!(decision.action, GhostPolicyAction::KeepDefault);
        assert_eq!(decision.selected_ghost, "coder");
        assert!(decision.explanation.candidate_stats.iter().any(|c| c
            .rejection_reasons
            .iter()
            .any(|r| r == "insufficient_samples")));
    }

    #[test]
    fn fallback_to_safe_default_for_noisy_context() {
        let mut noisy_thresholds = thresholds();
        noisy_thresholds.stability_window = 4;

        let decision = evaluate_ghost_policy(
            scope(),
            &["coder".into(), "scout".into()],
            "coder",
            None,
            noisy_thresholds,
            &[
                metrics("coder", 5, 4, 5, 4, 0),
                metrics("scout", 5, 5, 5, 5, 0),
            ],
            &[metrics("scout", 1, 1, 1, 1, 0)],
        );

        assert_eq!(decision.action, GhostPolicyAction::KeepDefault);
        assert_eq!(decision.selected_ghost, "coder");
        assert!(decision.explanation.candidate_stats.iter().any(|c| c
            .rejection_reasons
            .iter()
            .any(|r| r == "insufficient_recent_samples")));
    }

    #[test]
    fn rollback_when_active_candidate_regresses() {
        let decision = evaluate_ghost_policy(
            scope(),
            &["coder".into(), "scout".into()],
            "coder",
            Some("scout"),
            thresholds(),
            &[
                metrics("coder", 6, 5, 6, 5, 0),
                metrics("scout", 6, 2, 6, 2, 2),
            ],
            &[],
        );

        assert_eq!(
            decision.action,
            GhostPolicyAction::Rollback {
                to_baseline: "coder".to_string()
            }
        );
        assert_eq!(decision.selected_ghost, "coder");
        assert!(decision
            .explanation
            .reason_codes
            .iter()
            .any(|r| r == "candidate_regression_exceeded"));
    }

    #[test]
    fn explanation_payload_contains_scope_thresholds_and_reasons() {
        let decision = evaluate_ghost_policy(
            scope(),
            &["coder".into(), "scout".into()],
            "coder",
            None,
            thresholds(),
            &[
                metrics("coder", 5, 4, 5, 4, 0),
                metrics("scout", 5, 5, 5, 5, 0),
            ],
            &[],
        );

        assert_eq!(decision.explanation.scope.repo, "sparks");
        assert_eq!(decision.explanation.thresholds.min_samples, 3);
        assert!(!decision.explanation.reason_codes.is_empty());
        assert!(!decision.explanation.decided_at.is_empty());
        assert_eq!(decision.explanation.selected_ghost, decision.selected_ghost);
    }
}
