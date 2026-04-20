use std::sync::Arc;

/// Outcome of a completed spark run, passed to `after_spark_complete`.
pub enum SparkOutcome {
    Success(String),
    Failure(String),
}

/// Lifecycle hooks that run deterministically regardless of LLM behavior.
///
/// `before_model_call` runs inside the strategy loop before each LLM call.
/// `after_spark_complete` runs in `Executor::run()` after the strategy returns,
/// including on error paths — the safety net guarantee.
#[async_trait::async_trait]
pub trait SparkMiddleware: Send + Sync {
    async fn before_model_call(&self, session_id: &str, ghost: &str);
    async fn after_spark_complete(&self, session_id: &str, ghost: &str, outcome: &SparkOutcome);
}

/// Middleware that ensures the activity log is written even if a spark crashes.
///
/// Currently a no-op placeholder — the executor already flushes the log on
/// session close. This exists so the middleware infrastructure is exercised
/// end-to-end and the slot is ready for future flush logic.
pub struct ActivityLogFlushMiddleware;

#[async_trait::async_trait]
impl SparkMiddleware for ActivityLogFlushMiddleware {
    async fn before_model_call(&self, _session_id: &str, _ghost: &str) {}

    async fn after_spark_complete(&self, session_id: &str, ghost: &str, outcome: &SparkOutcome) {
        let status = match outcome {
            SparkOutcome::Success(_) => "success",
            SparkOutcome::Failure(_) => "failure",
        };
        tracing::debug!(session_id, ghost, status, "ActivityLogFlushMiddleware: spark complete");
    }
}

/// Convenience type alias used throughout the codebase.
pub type SharedMiddlewares = Vec<Arc<dyn SparkMiddleware>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingMiddleware {
        before_count: Arc<AtomicUsize>,
        after_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SparkMiddleware for CountingMiddleware {
        async fn before_model_call(&self, _session_id: &str, _ghost: &str) {
            self.before_count.fetch_add(1, Ordering::SeqCst);
        }
        async fn after_spark_complete(&self, _session_id: &str, _ghost: &str, _outcome: &SparkOutcome) {
            self.after_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn middleware_called_correct_count() {
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let mw: Arc<dyn SparkMiddleware> = Arc::new(CountingMiddleware {
            before_count: before.clone(),
            after_count: after.clone(),
        });

        mw.before_model_call("sess1", "coder").await;
        mw.before_model_call("sess1", "coder").await;
        mw.after_spark_complete("sess1", "coder", &SparkOutcome::Success("done".into())).await;

        assert_eq!(before.load(Ordering::SeqCst), 2);
        assert_eq!(after.load(Ordering::SeqCst), 1);
    }
}
