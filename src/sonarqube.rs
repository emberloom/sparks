//! SonarQube quality gate client.
//!
//! Polls the SonarQube API until the quality gate passes (or times out).
//! Used as a mandatory check in the CI pipeline before PRs are opened.

use serde::Deserialize;

use crate::config::SonarqubeConfig;

/// The status of a SonarQube quality gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateStatus {
    Ok,
    Warn,
    Error,
    None,
    Pending,
}

impl GateStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "OK" => Self::Ok,
            "WARN" => Self::Warn,
            "ERROR" => Self::Error,
            "NONE" => Self::None,
            _ => Self::Pending,
        }
    }

    pub fn is_passing(&self) -> bool {
        matches!(self, Self::Ok | Self::Warn)
    }
}

impl std::fmt::Display for GateStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "OK"),
            Self::Warn => write!(f, "WARN"),
            Self::Error => write!(f, "ERROR"),
            Self::None => write!(f, "NONE"),
            Self::Pending => write!(f, "PENDING"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiProjectStatus {
    status: String,
    conditions: Option<Vec<ApiCondition>>,
}

#[derive(Debug, Deserialize)]
struct ApiCondition {
    status: String,
    #[serde(rename = "metricKey")]
    metric_key: String,
    #[serde(rename = "actualValue", default)]
    actual_value: String,
    #[serde(rename = "errorThreshold", default)]
    error_threshold: String,
}

#[derive(Debug, Deserialize)]
struct ApiProjectStatusResponse {
    #[serde(rename = "projectStatus")]
    project_status: ApiProjectStatus,
}

/// A failed quality gate condition.
#[derive(Debug, Clone)]
pub struct FailedCondition {
    pub metric: String,
    pub actual: String,
    pub threshold: String,
}

/// Result of a quality gate check.
#[derive(Debug)]
pub struct GateResult {
    pub status: GateStatus,
    pub failed_conditions: Vec<FailedCondition>,
}

impl GateResult {
    pub fn is_passing(&self) -> bool {
        self.status.is_passing()
    }

    pub fn summary(&self) -> String {
        if self.is_passing() {
            format!("Quality gate: {} \u{2705}", self.status)
        } else {
            let conditions = self
                .failed_conditions
                .iter()
                .map(|c| {
                    format!(
                        "  \u{2022} {} = {} (threshold: {})",
                        c.metric, c.actual, c.threshold
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "Quality gate: {} \u{274c}\nFailed conditions:\n{}",
                self.status, conditions
            )
        }
    }
}

/// SonarQube API client.
pub struct SonarClient {
    http: reqwest::Client,
    config: SonarqubeConfig,
}

impl SonarClient {
    pub fn new(config: SonarqubeConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");
        Self { http, config }
    }

    fn token(&self) -> Option<String> {
        self.config
            .token
            .clone()
            .or_else(|| std::env::var("SPARKS_SONAR_TOKEN").ok())
    }

    /// Fetch the current quality gate status for the configured project.
    pub async fn get_gate_status(&self) -> anyhow::Result<GateResult> {
        let project_key = self
            .config
            .project_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("sonarqube.project_key is required"))?;

        let base_url = format!(
            "{}/api/qualitygates/project_status",
            self.config.server_url.trim_end_matches('/')
        );

        let mut request = self
            .http
            .get(&base_url)
            .query(&[("projectKey", project_key)]);

        if let Some(org) = &self.config.organization {
            request = request.query(&[("organization", org.as_str())]);
        }
        if let Some(token) = self.token() {
            // SonarQube uses HTTP Basic auth with the token as the username and an
            // empty password (i.e. "Authorization: Basic base64(token:)").
            // Bearer auth is NOT supported and will return 401.
            request = request.basic_auth(&token, Option::<&str>::None);
        }

        let resp = request.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("SonarQube API error {}: {}", status, body));
        }

        let data: ApiProjectStatusResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse SonarQube response: {}", e))?;

        let status = GateStatus::from_str(&data.project_status.status);
        let failed_conditions = data
            .project_status
            .conditions
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.status == "ERROR")
            .map(|c| FailedCondition {
                metric: c.metric_key,
                actual: c.actual_value,
                threshold: c.error_threshold,
            })
            .collect();

        Ok(GateResult {
            status,
            failed_conditions,
        })
    }

    /// Poll until the quality gate passes, times out, or fails definitively.
    pub async fn wait_for_gate(&self) -> anyhow::Result<GateResult> {
        let timeout = tokio::time::Duration::from_secs(self.config.gate_timeout_secs);
        let poll = tokio::time::Duration::from_secs(self.config.poll_interval_secs);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let result = self.get_gate_status().await?;

            match result.status {
                GateStatus::Pending => {
                    // Analysis still running; keep waiting
                }
                GateStatus::Ok | GateStatus::Warn => return Ok(result),
                GateStatus::Error | GateStatus::None => {
                    return Ok(result);
                }
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "SonarQube quality gate timed out after {}s",
                    self.config.gate_timeout_secs
                ));
            }
            tokio::time::sleep(poll).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_status_from_str_all_variants() {
        assert_eq!(GateStatus::from_str("OK"), GateStatus::Ok);
        assert_eq!(GateStatus::from_str("WARN"), GateStatus::Warn);
        assert_eq!(GateStatus::from_str("ERROR"), GateStatus::Error);
        assert_eq!(GateStatus::from_str("NONE"), GateStatus::None);
        assert_eq!(GateStatus::from_str("PENDING"), GateStatus::Pending);
        assert_eq!(GateStatus::from_str("IN_PROGRESS"), GateStatus::Pending);
        assert_eq!(GateStatus::from_str(""), GateStatus::Pending);
    }

    #[test]
    fn gate_status_is_passing() {
        assert!(GateStatus::Ok.is_passing());
        assert!(GateStatus::Warn.is_passing());
        assert!(!GateStatus::Error.is_passing());
        assert!(!GateStatus::None.is_passing());
        assert!(!GateStatus::Pending.is_passing());
    }

    #[test]
    fn gate_result_summary_passing() {
        let result = GateResult {
            status: GateStatus::Ok,
            failed_conditions: vec![],
        };
        let summary = result.summary();
        assert!(summary.contains("OK"));
        assert!(result.is_passing());
    }

    #[test]
    fn gate_result_summary_failing() {
        let result = GateResult {
            status: GateStatus::Error,
            failed_conditions: vec![
                FailedCondition {
                    metric: "coverage".to_string(),
                    actual: "45.2".to_string(),
                    threshold: "80.0".to_string(),
                },
                FailedCondition {
                    metric: "duplicated_lines_density".to_string(),
                    actual: "12.5".to_string(),
                    threshold: "3.0".to_string(),
                },
            ],
        };
        let summary = result.summary();
        assert!(summary.contains("ERROR"));
        assert!(summary.contains("Failed conditions"));
        assert!(summary.contains("coverage"));
        assert!(summary.contains("45.2"));
        assert!(summary.contains("80.0"));
        assert!(summary.contains("duplicated_lines_density"));
        assert!(!result.is_passing());
    }

    #[test]
    fn gate_status_display() {
        assert_eq!(GateStatus::Ok.to_string(), "OK");
        assert_eq!(GateStatus::Warn.to_string(), "WARN");
        assert_eq!(GateStatus::Error.to_string(), "ERROR");
        assert_eq!(GateStatus::None.to_string(), "NONE");
        assert_eq!(GateStatus::Pending.to_string(), "PENDING");
    }

    #[test]
    fn failed_condition_fields() {
        let c = FailedCondition {
            metric: "new_bugs".to_string(),
            actual: "3".to_string(),
            threshold: "0".to_string(),
        };
        assert_eq!(c.metric, "new_bugs");
        assert_eq!(c.actual, "3");
        assert_eq!(c.threshold, "0");
    }
}
