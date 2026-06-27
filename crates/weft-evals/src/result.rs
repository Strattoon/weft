use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Green,
    Red,
    Void,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeftEvalResult {
    pub schema_version: String,
    pub task_id: String,
    pub verdict: Verdict,
    /// One-line machine-and-human readable reason. Never the only signal.
    pub reason: String,
    pub oracle_command: String,
    pub oracle_exit_code: Option<i32>,
    /// The typed `Vec<Diagnostic>` from the in-process validator, serialized as
    /// JSON for provenance, or `null` on an instrument failure (=> void).
    pub diagnostics: serde_json::Value,
}

impl WeftEvalResult {
    pub const SCHEMA_VERSION: &'static str = "weft_eval_result_v0";

    pub fn new(task_id: &str, verdict: Verdict, reason: impl Into<String>, oracle_command: &str) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION.to_string(),
            task_id: task_id.to_string(),
            verdict,
            reason: reason.into(),
            oracle_command: oracle_command.to_string(),
            oracle_exit_code: None,
            diagnostics: serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_round_trips_with_versioned_schema() {
        let r = WeftEvalResult::new("t1", Verdict::Void, "oracle missing", "weft validate");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"schema_version\":\"weft_eval_result_v0\""));
        assert!(s.contains("\"verdict\":\"void\""));
    }
}
