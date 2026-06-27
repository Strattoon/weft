//! StateMachine surface types (pre-lowering AST).
//!
//! A `StateMachine` block is surface syntax that P6d lowers onto Weft's durable
//! While-loop spine (see `docs/state-machine-lowering.md`). These types are the
//! parse-time AST only: P6b (this module) populates them from the CST; P6c adds
//! the validation pass; P6d performs the lowering. NO validation or lowering
//! lives here — the parser extracts the literal surface shape and nothing more.
//!
//! The state set and event set are DERIVED (the union of all `from`/`to` and all
//! `event` respectively); they are not stored explicitly, mirroring Workday's
//! `TRANSITIONS` table over `RunState`/`RunEvent` enums.

use serde::{Deserialize, Serialize};

/// One parsed `StateMachine` block. The AST target for P6b, mirroring the AST
/// shape pinned in `docs/state-machine-lowering.md` §1.4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateMachineDef {
    /// The block name (`StateMachine <name> { ... }`).
    pub name: String,
    /// The initial state (an `IDENT`; must be in the derived state set — a P6c check).
    pub initial: String,
    /// Terminal states (the `terminal: [..]` list). Non-empty / subset-of-states
    /// is a P6c concern; the parser stores whatever was written.
    pub terminal: Vec<String>,
    /// Authority states (the `authority: [..]` list). A model-owned transition
    /// into any of these states is a P6c `sm-authority-inversion` error.
    /// Optional in the grammar — absent → empty vec (P6c checks run cleanly on
    /// an empty authority set: no inversions possible).
    #[serde(default)]
    pub authority: Vec<String>,
    /// The cycle-guard cap (`max_iters`), per `docs/state-machine-lowering.md` §2.
    pub max_iters: u32,
    /// The transition rows, in source order.
    pub transitions: Vec<SmTransition>,
}

/// One transition row: `(from, event) -> to owner=.. guard=.. artifact=..`.
/// Mirrors Workday's `Transition` 1:1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmTransition {
    /// Source state.
    pub from: String,
    /// Event symbol selecting this edge out of `from`.
    pub event: String,
    /// Target state.
    pub to: String,
    /// Disposing authority (closed vocabulary of four values).
    pub owner: SmOwner,
    /// Named guard-predicate label. Optional at the AST level; whether it is
    /// required/non-empty is a P6c check.
    pub guard: Option<String>,
    /// Optional required artifact name on the target state.
    pub artifact: Option<String>,
}

/// The disposing authority for a transition (the `owner=` attribute). A closed
/// vocabulary of four values; the surface spelling is lowercase
/// (`model`/`script`/`human`/`gateway`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmOwner {
    Script,
    Model,
    Human,
    Gateway,
}

impl SmOwner {
    /// Parse an `owner=` keyword from its surface spelling. Returns `None` for an
    /// unknown spelling (the parser stays lenient; a malformed owner becomes a
    /// diagnostic / dropped attribute rather than a panic).
    pub fn from_keyword(s: &str) -> Option<SmOwner> {
        match s {
            "script" => Some(SmOwner::Script),
            "model" => Some(SmOwner::Model),
            "human" => Some(SmOwner::Human),
            "gateway" => Some(SmOwner::Gateway),
            _ => None,
        }
    }
}
