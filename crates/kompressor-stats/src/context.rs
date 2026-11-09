//! Reproducible experiment identity and measured proxy phases. Neither stores
//! prompts, credentials, response bodies, or provider authorization settings.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunContext {
    pub build_version: String,
    pub config_fingerprint: String,
    pub experiment_id: Option<String>,
    pub run_id: String,
    /// An explicit allowlist of compression/cache/retrieval policy settings.
    pub settings: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProxyTiming {
    pub first_head_ms: Option<f64>,
    pub attempts_total: u64,
    pub attempts: Vec<UpstreamAttempt>,
    pub retrieval_calls: u64,
    pub retrieval_execution_ms: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpstreamAttempt {
    pub duration_ms: f64,
    pub status: Option<u16>,
    pub failed: bool,
}
impl ProxyTiming {
    pub fn record_attempt(
        &mut self,
        duration_ms: f64,
        elapsed_ms: f64,
        status: Option<u16>,
        failed: bool,
    ) {
        self.attempts_total += 1;
        if self.first_head_ms.is_none() && status.is_some() {
            self.first_head_ms = Some(elapsed_ms);
        }
        // A looping or failing upstream must not make telemetry unbounded.
        if self.attempts.len() < 64 {
            self.attempts.push(UpstreamAttempt {
                duration_ms,
                status,
                failed,
            });
        }
    }
    pub fn record_retrieval(&mut self, duration_ms: f64) {
        self.retrieval_calls += 1;
        self.retrieval_execution_ms += duration_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_head_excludes_failed_connects_and_attempt_details_are_bounded() {
        let mut t = ProxyTiming::default();
        t.record_attempt(10.0, 10.0, None, true);
        assert_eq!(t.first_head_ms, None);
        t.record_attempt(20.0, 35.0, Some(429), false);
        t.record_attempt(5.0, 42.0, Some(200), false);
        assert_eq!(t.first_head_ms, Some(35.0));
        for _ in 0..100 {
            t.record_attempt(1.0, 100.0, Some(200), false);
        }
        assert_eq!(t.attempts_total, 103);
        assert_eq!(t.attempts.len(), 64);
        t.record_retrieval(2.0);
        t.record_retrieval(3.0);
        assert_eq!(t.retrieval_execution_ms, 5.0);
        assert_eq!(t.retrieval_calls, 2);
    }
}
