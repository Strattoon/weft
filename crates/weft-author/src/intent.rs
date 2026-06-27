use crate::author::Author;
use crate::catalog_index::NodeIndex;
use crate::spec::AuthoringSpec;
use anyhow::Result;

/// Intent layer: free-form chat -> prose spec. Lexical pre-selection shortlists
/// candidate nodes; the (smart) model emits an AuthoringSpec as JSON confirming
/// goal/steps/nodes/io. The caller shows spec.to_markdown() to the human for
/// readback BEFORE authoring — intent correctness is a human gate, not a compiler one.
pub fn derive_spec(author: &dyn Author, index: &NodeIndex, chat: &str) -> Result<AuthoringSpec> {
    let shortlist = index.select(chat, 8);
    let prompt = format!(
        "User request: {chat}\nCandidate node types: {}\n\
         Reply with ONLY JSON: {{\"goal\":..,\"steps\":[..],\"selected_nodes\":[..],\"io\":..}}",
        shortlist.join(", ")
    );
    // The intent model's `propose` returns the JSON spec (not .weft).
    let json = author.propose(&prompt)?;
    let spec: AuthoringSpec = serde_json::from_str(json.trim())
        .map_err(|e| anyhow::anyhow!("intent model did not return a valid AuthoringSpec: {e}"))?;
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::author::MockAuthor;
    use crate::catalog_index::NodeIndex;
    use crate::test_support::{node, FakeCat};

    fn make_index() -> (FakeCat, NodeIndex) {
        let cat = FakeCat(vec![
            node(
                "SendEmail",
                "send an email message to a recipient",
                &["email", "notify"],
            ),
            node(
                "HumanTrigger",
                "trigger a workflow when a human submits a form",
                &["trigger", "form", "submit"],
            ),
        ]);
        let idx = NodeIndex::build(&cat);
        (cat, idx)
    }

    #[test]
    fn derive_spec_parses_model_json_into_prose_spec() {
        let (_cat, idx) = make_index();
        let model = MockAuthor::new(vec![
            r#"{"goal":"email on submit","steps":["trigger","send"],"selected_nodes":["HumanTrigger","SendEmail"],"io":"form->email"}"#
                .to_string(),
        ]);
        let spec = derive_spec(&model, &idx, "email me when someone submits the form").unwrap();
        assert_eq!(spec.goal, "email on submit");
        assert!(spec.selected_nodes.contains(&"SendEmail".to_string()));
    }

    #[test]
    fn derive_spec_fails_loud_on_malformed_json() {
        let (_cat, idx) = make_index();
        let model = MockAuthor::new(vec!["not json".to_string()]);
        let result = derive_spec(&model, &idx, "email me when someone submits the form");
        assert!(result.is_err(), "expected Err on malformed JSON, got Ok");
    }
}
