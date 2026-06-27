use weft_core::node::{Diagnostic, Severity};

use crate::result::Verdict;
use crate::task::Expectation;

/// Pure verdict over the in-process validator's typed diagnostics. Returns Green
/// or Red only; Void is the runner's job (instrument failure). Rule: diagnostics
/// MATCH the expectation ⇒ Green (candidate did the task); any mismatch ⇒ Red.
/// "Red" = candidate failed the task, never "the validator emitted a diagnostic."
pub fn score(exp: &Expectation, diagnostics: &[Diagnostic]) -> (Verdict, String) {
    let error_count = diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
    let codes: Vec<&str> = diagnostics.iter().filter_map(|d| d.code.as_deref()).collect();

    if exp.diagnostics_empty && error_count > 0 {
        return (Verdict::Red, format!("expected no errors, got {error_count}"));
    }
    let missing: Vec<&str> = exp.must_contain_codes.iter().map(|s| s.as_str())
        .filter(|c| !codes.contains(c)).collect();
    if !missing.is_empty() {
        return (Verdict::Red, format!("missing required codes: {missing:?}"));
    }
    let forbidden: Vec<&str> = exp.must_not_contain_codes.iter().map(|s| s.as_str())
        .filter(|c| codes.contains(c)).collect();
    if !forbidden.is_empty() {
        return (Verdict::Red, format!("forbidden codes present: {forbidden:?}"));
    }
    (Verdict::Green, "diagnostics match expectation".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::project::Span;

    fn diag(severity: Severity, code: &str) -> Diagnostic {
        Diagnostic::at(Span::default(), severity, code, "msg")
    }
    fn exp_clean() -> Expectation {
        Expectation { diagnostics_empty: true, must_contain_codes: vec![], must_not_contain_codes: vec![] }
    }

    #[test]
    fn clean_diagnostics_is_green() {
        assert_eq!(score(&exp_clean(), &[]).0, Verdict::Green);
    }

    #[test]
    fn errors_when_expecting_clean_is_red() {
        assert_eq!(score(&exp_clean(), &[diag(Severity::Error, "E_PORT")]).0, Verdict::Red);
    }

    #[test]
    fn warning_only_still_green_when_expecting_clean() {
        assert_eq!(score(&exp_clean(), &[diag(Severity::Warning, "W_X")]).0, Verdict::Green);
    }

    // Negative fixture: a CORRECT candidate is one that trips the check.
    #[test]
    fn negative_fixture_green_when_required_code_present() {
        let exp = Expectation { diagnostics_empty: false, must_contain_codes: vec!["tagged-flow-violation".into()], must_not_contain_codes: vec![] };
        assert_eq!(score(&exp, &[diag(Severity::Error, "tagged-flow-violation")]).0, Verdict::Green);
    }

    #[test]
    fn negative_fixture_red_when_required_code_missing() {
        let exp = Expectation { diagnostics_empty: false, must_contain_codes: vec!["tagged-flow-violation".into()], must_not_contain_codes: vec![] };
        assert_eq!(score(&exp, &[]).0, Verdict::Red);
    }
}
