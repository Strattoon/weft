use weft_core::node::{Diagnostic, MetadataCatalog, Severity};

use crate::author::Author;
use crate::context;

#[derive(Debug, PartialEq)]
pub enum AuthorStatus {
    Green,
    ExhaustedRed,
    Error,
}

pub struct AuthorOutcome {
    pub weft: String,
    pub rounds: u32,
    pub status: AuthorStatus,
}

/// propose -> validate -> repair, bounded by max_rounds. The validator is the
/// deterministic weft-evals gate (injected so tests need no real catalog). Green
/// = no error diagnostics. The model proposes; the gate decides.
pub fn author_until_green(
    author: &dyn Author,
    catalog: &dyn MetadataCatalog,
    selected: &[String],
    spec_md: &str,
    validate: &dyn Fn(&str) -> Result<Vec<Diagnostic>, String>,
    max_rounds: u32,
) -> AuthorOutcome {
    let card = crate::grammar_card::card();
    let mut diagnostics = String::new();
    let mut prev = String::new();

    for round in 1..=max_rounds {
        let ctx = context::build(card, catalog, selected, spec_md, &diagnostics);

        let weft = if round == 1 {
            match author.propose(&ctx) {
                Ok(w) => w,
                Err(_) => {
                    return AuthorOutcome {
                        weft: prev,
                        rounds: round,
                        status: AuthorStatus::Error,
                    }
                }
            }
        } else {
            match author.repair(&ctx, &prev, &diagnostics) {
                Ok(w) => w,
                Err(_) => {
                    return AuthorOutcome {
                        weft: prev,
                        rounds: round,
                        status: AuthorStatus::Error,
                    }
                }
            }
        };

        match validate(&weft) {
            Ok(diags) => {
                let errs: Vec<&Diagnostic> = diags
                    .iter()
                    .filter(|d| d.severity == Severity::Error)
                    .collect();

                if errs.is_empty() {
                    return AuthorOutcome {
                        weft,
                        rounds: round,
                        status: AuthorStatus::Green,
                    };
                }

                diagnostics = errs
                    .iter()
                    .map(|d| {
                        format!(
                            "L{}:{} [{}] {}",
                            d.line,
                            d.column,
                            d.code.as_deref().unwrap_or(""),
                            d.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                prev = weft;
            }
            Err(_) => {
                return AuthorOutcome {
                    weft,
                    rounds: round,
                    status: AuthorStatus::Error,
                }
            }
        }
    }

    AuthorOutcome {
        weft: prev,
        rounds: max_rounds,
        status: AuthorStatus::ExhaustedRed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::author::MockAuthor;
    use crate::test_support::{node, FakeCat};
    use weft_core::node::Severity;
    use weft_core::project::Span;

    fn make_cat() -> FakeCat {
        FakeCat(vec![node(
            "SendEmail",
            "send an email message to a recipient",
            &["email", "notify"],
        )])
    }

    fn error_diag() -> Diagnostic {
        Diagnostic::at(
            Span::default(),
            Severity::Error,
            "type-mismatch",
            "expected String got Number",
        )
    }

    #[test]
    fn converges_after_one_repair() {
        // round 1 proposes "bad" → validator returns one error;
        // round 2 repairs → "good" → validator returns Ok(vec![]) → Green
        let model = MockAuthor::new(vec!["bad".into(), "good".into()]);
        let cat = make_cat();

        let validate = |src: &str| -> Result<Vec<Diagnostic>, String> {
            if src == "good" {
                Ok(vec![])
            } else {
                Ok(vec![error_diag()])
            }
        };

        let out = author_until_green(
            &model,
            &cat,
            &["SendEmail".to_string()],
            "send an email spec",
            &validate,
            5,
        );

        assert_eq!(out.status, AuthorStatus::Green);
        assert_eq!(out.rounds, 2);
    }

    #[test]
    fn green_on_first_round() {
        let model = MockAuthor::new(vec!["good".into()]);
        let cat = make_cat();

        let validate = |_src: &str| -> Result<Vec<Diagnostic>, String> { Ok(vec![]) };

        let out = author_until_green(
            &model,
            &cat,
            &["SendEmail".to_string()],
            "spec",
            &validate,
            5,
        );

        assert_eq!(out.status, AuthorStatus::Green);
        assert_eq!(out.rounds, 1);
    }

    #[test]
    fn exhausted_red_when_never_green() {
        let model = MockAuthor::new(vec!["bad".into(), "bad".into(), "bad".into()]);
        let cat = make_cat();

        let validate = |_src: &str| -> Result<Vec<Diagnostic>, String> {
            Ok(vec![error_diag()])
        };

        let out = author_until_green(
            &model,
            &cat,
            &["SendEmail".to_string()],
            "spec",
            &validate,
            3,
        );

        assert_eq!(out.status, AuthorStatus::ExhaustedRed);
        assert_eq!(out.rounds, 3);
    }

    #[test]
    fn error_when_validate_errs() {
        let model = MockAuthor::new(vec!["anything".into()]);
        let cat = make_cat();

        let validate =
            |_src: &str| -> Result<Vec<Diagnostic>, String> { Err("validator crashed".into()) };

        let out = author_until_green(
            &model,
            &cat,
            &["SendEmail".to_string()],
            "spec",
            &validate,
            5,
        );

        assert_eq!(out.status, AuthorStatus::Error);
    }
}
