/// Dense grammar cheatsheet compressed from docs/weft-lang-guide.md. Token-
/// efficient — forms + the closed validation-rule/error-code list, no prose.
pub const GRAMMAR_CARD: &str = include_str!("grammar_card.txt");

pub fn card() -> &'static str {
    GRAMMAR_CARD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_covers_the_core_forms_and_error_codes() {
        let c = card();
        for token in ["= NodeType", "->", ".in = ", "List[", "type-mismatch", "unresolved-typevar"] {
            assert!(c.contains(token), "grammar card missing `{token}`");
        }
    }
}
