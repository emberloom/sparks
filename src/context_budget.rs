//! Adaptive Context Budgeting
//!
//! Gates classifier context assembly based on input complexity. Short or
//! non-code requests skip expensive sources (KPI, lessons, memory search),
//! while code-heavy requests retain full context. This avoids wasting tokens
//! on context that won't affect the routing decision.

use std::time::Instant;

/// Pre-classification budget: lightweight context for the classifier itself.
/// The classifier needs enough context to decide, but doesn't need KPI history
/// or full memory search for simple requests.
#[derive(Debug, Clone)]
pub struct ClassifierContextPlan {
    pub recent_turns_limit: usize,
    pub memory_limit: usize,
    pub load_kpi: bool,
    pub load_lessons: bool,
}

impl Default for ClassifierContextPlan {
    fn default() -> Self {
        Self {
            recent_turns_limit: 20,
            memory_limit: 10,
            load_kpi: true,
            load_lessons: true,
        }
    }
}

/// Infer a lightweight classifier plan from input length and recency.
/// Short, simple inputs get cheaper classifier context.
pub fn infer_classifier_plan(user_input: &str, turn_count: usize) -> ClassifierContextPlan {
    let input_len = user_input.len();
    let has_code_keywords = contains_code_keywords(user_input);

    if input_len < 40 && !has_code_keywords && turn_count < 5 {
        // Very short input, no code keywords, early in conversation
        ClassifierContextPlan {
            recent_turns_limit: 6,
            memory_limit: 3,
            load_kpi: false,
            load_lessons: false,
        }
    } else if !has_code_keywords {
        // Longer input but no code keywords
        ClassifierContextPlan {
            recent_turns_limit: 12,
            memory_limit: 5,
            load_kpi: false,
            load_lessons: true,
        }
    } else {
        ClassifierContextPlan::default()
    }
}

/// Check if user input contains keywords suggesting code/tool operations.
fn contains_code_keywords(input: &str) -> bool {
    let lower = input.to_lowercase();
    const KEYWORDS: &[&str] = &[
        "code",
        "implement",
        "refactor",
        "fix",
        "bug",
        "build",
        "deploy",
        "test",
        "write",
        "edit",
        "modify",
        "create",
        "delete",
        "git",
        "cargo",
        "npm",
        "docker",
        "file",
        "function",
        "struct",
        "class",
        "module",
        "import",
        "compile",
        "lint",
        "pr",
        "merge",
        "branch",
        "commit",
        "push",
        "pull",
    ];
    KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Track context assembly timings for observability.
#[derive(Debug, Clone)]
pub struct ContextAssemblyMetrics {
    pub memory_search_ms: Option<u128>,
    pub embedding_ms: Option<u128>,
    pub kpi_ms: Option<u128>,
    pub total_ms: u128,
    pub sources_loaded: usize,
    pub sources_skipped: usize,
}

impl ContextAssemblyMetrics {
    pub fn start() -> ContextAssemblyTimer {
        ContextAssemblyTimer {
            started: Instant::now(),
            memory_search_ms: None,
            embedding_ms: None,
            kpi_ms: None,
            sources_loaded: 0,
            sources_skipped: 0,
        }
    }
}

/// Builder for assembling context metrics during the context loading phase.
pub struct ContextAssemblyTimer {
    started: Instant,
    memory_search_ms: Option<u128>,
    embedding_ms: Option<u128>,
    kpi_ms: Option<u128>,
    sources_loaded: usize,
    sources_skipped: usize,
}

impl ContextAssemblyTimer {
    pub fn record_memory_search(&mut self, ms: u128) {
        self.memory_search_ms = Some(ms);
    }

    pub fn record_embedding(&mut self, ms: u128) {
        self.embedding_ms = Some(ms);
    }

    pub fn record_kpi(&mut self, ms: u128) {
        self.kpi_ms = Some(ms);
    }

    pub fn record_loaded(&mut self) {
        self.sources_loaded += 1;
    }

    pub fn record_skipped(&mut self) {
        self.sources_skipped += 1;
    }

    pub fn finish(self) -> ContextAssemblyMetrics {
        ContextAssemblyMetrics {
            memory_search_ms: self.memory_search_ms,
            embedding_ms: self.embedding_ms,
            kpi_ms: self.kpi_ms,
            total_ms: self.started.elapsed().as_millis(),
            sources_loaded: self.sources_loaded,
            sources_skipped: self.sources_skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_plan_cheap_for_short_input() {
        let plan = infer_classifier_plan("hello", 2);
        assert!(!plan.load_kpi);
        assert!(!plan.load_lessons);
        assert_eq!(plan.memory_limit, 3);
    }

    #[test]
    fn classifier_plan_full_for_code_input() {
        let plan = infer_classifier_plan("implement a new auth module", 2);
        assert!(plan.load_kpi);
        assert!(plan.load_lessons);
        assert_eq!(plan.memory_limit, 10);
    }

    #[test]
    fn code_keywords_detected() {
        assert!(contains_code_keywords("fix the login bug"));
        assert!(contains_code_keywords("git push origin main"));
        assert!(contains_code_keywords("Implement new feature"));
        assert!(!contains_code_keywords("hello how are you"));
        assert!(!contains_code_keywords("what's the weather"));
    }

    #[test]
    fn context_assembly_timer_tracks_metrics() {
        let mut timer = ContextAssemblyMetrics::start();
        timer.record_memory_search(50);
        timer.record_embedding(30);
        timer.record_loaded();
        timer.record_loaded();
        timer.record_skipped();
        let metrics = timer.finish();
        assert_eq!(metrics.memory_search_ms, Some(50));
        assert_eq!(metrics.embedding_ms, Some(30));
        assert_eq!(metrics.sources_loaded, 2);
        assert_eq!(metrics.sources_skipped, 1);
    }
}
