use crate::author::Author;
use crate::catalog_index::NodeIndex;
use crate::spec::AuthoringSpec;
use anyhow::Result;

/// Coerce a `serde_json::Value` to a plain `String`.
/// - If it's already a string, return it as-is.
/// - Otherwise compact-serialize it (handles objects, arrays, numbers, bools, null).
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Coerce a `serde_json::Value` array element to a human-readable string.
/// Prefers a named string field (`step`, `description`, `name`, `node`, `type`)
/// inside an object; falls back to `value_to_string`.
fn array_elem_to_string(v: &serde_json::Value) -> String {
    if let serde_json::Value::Object(map) = v {
        for key in &["step", "description", "name", "node", "type"] {
            if let Some(serde_json::Value::String(s)) = map.get(*key) {
                return s.clone();
            }
        }
    }
    value_to_string(v)
}

/// Extract the first complete top-level JSON object from `s` via brace-depth
/// matching that respects string literals and escapes.
///
/// Real models often wrap the intent JSON in markdown fences (` ```json … ``` `)
/// or append a prose explanation after the object (`{…}\n\nHere's the workflow…`).
/// Whole-response fence stripping in the provider only handles the clean case;
/// this recovers the object whenever it is embedded in surrounding text.
/// Returns `None` if there is no balanced `{ … }` object.
fn extract_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Tolerant parser: accepts shape variance from real models.
///
/// Only returns `Err` when no balanced JSON object can be recovered at all
/// (keeps fail-loud behaviour for truly-not-JSON output).
fn parse_spec_tolerant(json: &str) -> Result<AuthoringSpec> {
    // First try the text as-is; if that fails, try to recover an embedded
    // object (handles leftover fences / trailing prose around the JSON).
    let v: serde_json::Value = serde_json::from_str(json)
        .or_else(|first_err| {
            extract_json_object(json)
                .map(serde_json::from_str)
                .unwrap_or(Err(first_err))
        })
        .map_err(|e| anyhow::anyhow!("intent model did not return valid JSON: {e}"))?;

    // goal: string or compact-stringify; default ""
    let goal = match v.get("goal") {
        Some(g) => value_to_string(g),
        None => String::new(),
    };

    // steps: array of strings (each element coerced); single string wrapped; missing → []
    let steps = match v.get("steps") {
        Some(serde_json::Value::Array(arr)) => {
            arr.iter().map(array_elem_to_string).collect()
        }
        Some(s @ serde_json::Value::String(_)) => vec![value_to_string(s)],
        Some(other) => vec![value_to_string(other)],
        None => vec![],
    };

    // selected_nodes: array of strings; missing → []
    let selected_nodes = match v.get("selected_nodes") {
        Some(serde_json::Value::Array(arr)) => {
            arr.iter().map(array_elem_to_string).collect()
        }
        Some(s @ serde_json::Value::String(_)) => vec![value_to_string(s)],
        Some(other) => vec![value_to_string(other)],
        None => vec![],
    };

    // io: string use as-is; object/array/other → compact-stringify; missing → ""
    let io = match v.get("io") {
        Some(i) => value_to_string(i),
        None => String::new(),
    };

    Ok(AuthoringSpec { goal, steps, selected_nodes, io })
}

/// Intent layer: free-form chat -> prose spec. Lexical pre-selection shortlists
/// candidate nodes; the (smart) model emits an AuthoringSpec as JSON confirming
/// goal/steps/nodes/io. The caller shows spec.to_markdown() to the human for
/// readback BEFORE authoring — intent correctness is a human gate, not a compiler one.
pub fn derive_spec(author: &dyn Author, index: &NodeIndex, chat: &str) -> Result<AuthoringSpec> {
    let shortlist = index.select(chat, 8);
    let prompt = format!(
        "User request: {chat}\nCandidate node types: {}\n\
         Reply with ONLY JSON matching this exact schema — all fields required:\n\
         {{\"goal\":\"<plain string>\",\"steps\":[\"<string>\",..],\
         \"selected_nodes\":[\"<node-type string>\",..],\"io\":\"<plain string>\"}}\n\
         `goal` and `io` MUST be plain strings. `steps` MUST be an array of strings. \
         `selected_nodes` MUST be an array of node-type name strings.",
        shortlist.join(", ")
    );
    // The intent model's `propose` returns the JSON spec (not .weft).
    let json = author.propose(&prompt)?;
    parse_spec_tolerant(json.trim())
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

    #[test]
    fn derive_spec_coerces_object_io() {
        // Real-model failure: `io` returned as an object instead of a string.
        let (_cat, idx) = make_index();
        let model = MockAuthor::new(vec![
            r#"{"goal":"g","steps":["a","b"],"selected_nodes":["Debug"],"io":{"in":"x","out":"y"}}"#
                .to_string(),
        ]);
        let spec = derive_spec(&model, &idx, "email me when someone submits the form")
            .expect("tolerant parser must succeed when io is an object");
        assert!(
            spec.io.contains("in") || spec.io.contains("out"),
            "coerced io must contain original keys, got: {:?}",
            spec.io
        );
    }

    #[test]
    fn derive_spec_recovers_json_with_trailing_prose() {
        // Real gemini failure: valid JSON object followed by an explanation.
        let (_cat, idx) = make_index();
        let model = MockAuthor::new(vec![
            "{\"goal\":\"g\",\"steps\":[\"a\"],\"selected_nodes\":[\"Debug\"],\"io\":\"x\"}\n\nHere's the workflow you asked for."
                .to_string(),
        ]);
        let spec = derive_spec(&model, &idx, "anything")
            .expect("must recover object when prose trails the JSON");
        assert_eq!(spec.goal, "g");
    }

    #[test]
    fn derive_spec_recovers_fenced_json_with_trailing_prose() {
        // Fence whose closing line is followed by prose, so whole-response
        // fence stripping does not fire and a `` ```json `` prefix remains.
        let (_cat, idx) = make_index();
        let model = MockAuthor::new(vec![
            "```json\n{\"goal\":\"g2\",\"steps\":[],\"selected_nodes\":[],\"io\":\"\"}\n```\nDone!"
                .to_string(),
        ]);
        let spec = derive_spec(&model, &idx, "anything")
            .expect("must recover object from fence-plus-prose");
        assert_eq!(spec.goal, "g2");
    }

    #[test]
    fn extract_json_object_respects_braces_in_strings() {
        // A `}` inside a string value must not end the object early.
        let s = "noise {\"k\":\"a}b\",\"n\":1} tail";
        assert_eq!(
            super::extract_json_object(s),
            Some("{\"k\":\"a}b\",\"n\":1}")
        );
    }

    #[test]
    fn derive_spec_coerces_object_steps() {
        // Model returns steps as an array of objects instead of strings.
        let (_cat, idx) = make_index();
        let model = MockAuthor::new(vec![
            r#"{"goal":"g","steps":[{"step":"trigger the form"},{"step":"send email"}],"selected_nodes":["HumanTrigger","SendEmail"],"io":"form->email"}"#
                .to_string(),
        ]);
        let spec = derive_spec(&model, &idx, "email me when someone submits the form")
            .expect("tolerant parser must succeed when steps are objects");
        assert_eq!(spec.steps.len(), 2, "must have 2 steps");
        for s in &spec.steps {
            assert!(!s.is_empty(), "each coerced step must be non-empty");
        }
    }
}
