/// Shared test helpers reused by WA2, WA3, WA5, WA6 tests.
/// Compiled only for test builds (`#[cfg(test)]` in lib.rs).
use weft_core::node::{
    MetadataCatalog, NodeFeatures, NodeMetadata,
};

/// Build a minimal NodeMetadata for testing. Fills only the fields
/// needed to exercise catalog_index / context — every other field
/// gets its natural zero/empty value.
pub fn node(ty: &str, desc: &str, tags: &[&str]) -> NodeMetadata {
    NodeMetadata {
        node_type: ty.to_string(),
        label: ty.to_string(),
        description: desc.to_string(),
        category: "test".to_string(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
        icon: None,
        color: None,
        inputs: vec![],
        outputs: vec![],
        fields: vec![],
        requires_infra: false,
        images: vec![],
        features: NodeFeatures::default(),
        validate: vec![],
        form_field_specs_ref: None,
    }
}

/// Tiny in-memory MetadataCatalog backed by a Vec.
pub struct FakeCat(pub Vec<NodeMetadata>);

impl MetadataCatalog for FakeCat {
    fn lookup(&self, t: &str) -> Option<&NodeMetadata> {
        self.0.iter().find(|m| m.node_type == t)
    }
    fn all(&self) -> Vec<&NodeMetadata> {
        self.0.iter().collect()
    }
}
