//! The Weft authoring harness (generator side). Free-form chat -> prose spec
//! (intent layer) -> .weft authored + repaired against the weft-evals gate
//! (authoring layer). Depends one-way on weft-evals/weft-catalog; all providers
//! live here, never in the deterministic core.

pub mod grammar_card;
pub mod catalog_index;

#[cfg(test)]
pub(crate) mod test_support;
pub mod context;
pub mod spec;
pub mod author;
pub mod intent;
pub mod authoring;
pub mod bench;

#[cfg(feature = "providers")]
pub mod providers;
