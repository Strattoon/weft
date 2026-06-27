use weft_core::node::MetadataCatalog;

/// One compact spec line per selected node: `Type(in: T, ...) -> (out: T, ...)`
/// — description [tags]`. Only the SELECTED nodes — never the whole catalog.
pub fn node_specs(catalog: &dyn MetadataCatalog, selected: &[String]) -> String {
    let mut out = String::new();
    for ty in selected {
        let Some(m) = catalog.lookup(ty) else { continue };
        let ins: Vec<String> = m
            .inputs
            .iter()
            .map(|p| format!("{}: {}", p.name, p.port_type))
            .collect();
        let outs: Vec<String> = m
            .outputs
            .iter()
            .map(|p| format!("{}: {}", p.name, p.port_type))
            .collect();
        out.push_str(&format!(
            "{}({}) -> ({}) — {} {:?}\n",
            m.node_type,
            ins.join(", "),
            outs.join(", "),
            m.description,
            m.tags
        ));
    }
    out
}

/// Assemble the per-prompt context pack: grammar card + selected node specs +
/// the prose spec + (on repair) diagnostics. Token-efficient by construction —
/// only selected nodes, dense card, no language-guide prose.
pub fn build(
    card: &str,
    catalog: &dyn MetadataCatalog,
    selected: &[String],
    spec_md: &str,
    diagnostics: &str,
) -> String {
    let mut s = String::new();
    s.push_str("## GRAMMAR\n");
    s.push_str(card);
    s.push('\n');
    s.push_str("## AVAILABLE NODES\n");
    s.push_str(&node_specs(catalog, selected));
    s.push('\n');
    s.push_str("## SPEC\n");
    s.push_str(spec_md);
    s.push('\n');
    if !diagnostics.is_empty() {
        s.push_str("## FIX THESE DIAGNOSTICS\n");
        s.push_str(diagnostics);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar_card::card;
    use crate::test_support::{node, FakeCat};
    use weft_core::node::{NodeMetadata, PortDef};
    use weft_core::weft_type::WeftType;

    fn node_with_ports(ty: &str, desc: &str, tags: &[&str]) -> NodeMetadata {
        let mut m = node(ty, desc, tags);
        m.inputs = vec![PortDef {
            name: "recipient".to_string(),
            port_type: WeftType::Primitive(weft_core::weft_type::WeftPrimitive::String),
            required: true,
            configurable: false,
            produces_tags: vec![],
            forbids_tags: vec![],
        }];
        m.outputs = vec![PortDef {
            name: "result".to_string(),
            port_type: WeftType::Primitive(weft_core::weft_type::WeftPrimitive::Boolean),
            required: false,
            configurable: false,
            produces_tags: vec![],
            forbids_tags: vec![],
        }];
        m
    }

    #[test]
    fn pack_includes_only_selected_nodes_and_omits_empty_diagnostics() {
        let cat = FakeCat(vec![
            node_with_ports(
                "SendEmail",
                "send an email message to a recipient",
                &["email", "notify"],
            ),
            node("Cron", "fire on a schedule", &["timer", "schedule"]),
        ]);

        // node_specs: only selected node present
        let specs = node_specs(&cat, &["SendEmail".to_string()]);
        assert!(
            specs.contains("SendEmail("),
            "node_specs should contain 'SendEmail(': {specs}"
        );
        assert!(
            specs.contains("recipient: String"),
            "node_specs should render input port type: {specs}"
        );
        assert!(
            specs.contains("result: Boolean"),
            "node_specs should render output port type: {specs}"
        );
        assert!(
            !specs.contains("Cron"),
            "node_specs must NOT contain unselected node 'Cron': {specs}"
        );

        // build: contains grammar card content + selected node + spec text, omits diags section
        let pack = build(
            card(),
            &cat,
            &["SendEmail".to_string()],
            "send an email",
            "",
        );
        assert!(
            pack.contains("## GRAMMAR"),
            "pack should contain grammar section: {pack}"
        );
        assert!(
            pack.contains("= NodeType"),
            "pack should contain grammar card token '= NodeType': {pack}"
        );
        assert!(
            pack.contains("SendEmail("),
            "pack should contain selected node: {pack}"
        );
        assert!(
            pack.contains("send an email"),
            "pack should contain spec text: {pack}"
        );
        assert!(
            !pack.contains("FIX THESE DIAGNOSTICS"),
            "pack must NOT contain diagnostics section when diagnostics is empty: {pack}"
        );

        // When diagnostics is non-empty, the section IS present
        let pack_with_diags = build(
            card(),
            &cat,
            &["SendEmail".to_string()],
            "send an email",
            "L3:0 [type-mismatch] expected String got Number",
        );
        assert!(
            pack_with_diags.contains("FIX THESE DIAGNOSTICS"),
            "pack should contain diagnostics section when diagnostics is non-empty: {pack_with_diags}"
        );
        assert!(
            pack_with_diags.contains("type-mismatch"),
            "pack should include the diagnostic text: {pack_with_diags}"
        );
    }
}
