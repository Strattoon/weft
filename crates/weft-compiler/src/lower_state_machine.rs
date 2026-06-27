//! P6d: lower a validated `StateMachineDef` onto Weft's durable While-loop spine.
//!
//! A `StateMachine` is not a new executor (see `docs/state-machine-lowering.md`):
//! it lowers to the existing sequential `Loop` runtime. The loop's carry value
//! `current_state` is the active state; the body dispatches on it; a transition
//! selector updates the carry and votes `done` on a terminal state. Everything
//! durable (journal-fold resume, crash-replay idempotency, cancellation) is
//! inherited from the loop runtime unchanged.
//!
//! THIS MODULE EMITS THE CONTROL SKELETON ONLY (§2 / §3 of the design doc):
//! - one `GroupKind::Loop` group with the canonical While `LoopConfig`,
//! - `LoopIn` / `LoopOut` boundary nodes mirrored from the real loop flatten
//!   (`weft_compiler.rs::flatten_group`),
//! - a constant seed node emitting `sm.initial`, wired to the `current_state`
//!   carry input on `LoopIn` (§2.3 carry seed),
//! - a placeholder dispatcher keyed on `current_state` and a transition-selector
//!   node that writes `current_state = to` (carry) and `done = (to ∈ terminal)`
//!   (reserved port) onto `LoopOut` (§3 wiring — the binding requirement).
//!
//! OUT OF SCOPE (P6e): real per-state Workday/authority domain work nodes, the
//! tagged-flow `produces_tags`/`forbids_tags` on the lowered ports, and any
//! engine / Restate run. The router/per-state subgraph here is a generic
//! placeholder; only the carry/done LoopOut wiring is the runtime contract.

use weft_core::node::NodeFeatures;
use weft_core::project::{
    Edge, GroupBoundary, GroupBoundaryRole, GroupDefinition, GroupKind, NodeDefinition, Position,
    PortDefinition,
};
use weft_core::state_machine::StateMachineDef;
use weft_core::weft_type::{WeftPrimitive, WeftType};

/// The single carry port: the SM's current-state register (§2.1).
const CARRY: &str = "current_state";

/// Lower a (already-validated) `StateMachineDef` into the loop-graph IR fragment:
/// the boundary + body nodes, the carry/done/seed edges, and the one `Loop`
/// group. The caller folds these into a `ProjectDefinition` (see
/// `compile_with_state_machines`).
///
/// The fragment is self-contained and namespaced under the SM name so several
/// SMs in one file cannot collide. The emitted loop passes `check_loop_config`
/// as a legal While shape (`lower_state_machine_tests::lowered_loop_is_a_legal_while_shape`).
pub fn lower_state_machine(
    sm: &StateMachineDef,
) -> (Vec<NodeDefinition>, Vec<Edge>, Vec<GroupDefinition>) {
    // Loop group id, namespaced by the SM name so multiple SMs don't collide.
    let group_id = format!("sm__{}", sm.name);
    let in_id = format!("{group_id}__in");
    let out_id = format!("{group_id}__out");
    let seed_id = format!("{group_id}__seed");
    let dispatch_id = format!("{group_id}__dispatch");
    let selector_id = format!("{group_id}__selector");

    let string_ty = || WeftType::primitive(WeftPrimitive::String);
    let bool_ty = || WeftType::primitive(WeftPrimitive::Boolean);
    let number_ty = || WeftType::primitive(WeftPrimitive::Number);

    // --- LoopIn config: the canonical While LoopConfig (§2.1). -----------------
    // Field set + key spellings mirror what `flatten_group` materializes onto a
    // real loop's LoopIn (parentId + the five knobs). `parallel` is explicit
    // (the runtime never holds its own default; validate requires it present).
    let in_config = serde_json::json!({
        "parentId": group_id,
        "parallel": false,
        "over": [],
        "carry": [CARRY],
        "max_iters": sm.max_iters,
        "trim_on_mismatch": true,
    });

    // --- LoopIn boundary node. -------------------------------------------------
    // inputs: the outer-in carry seed port `current_state: String` (the engine
    //   seeds carry from the loop's same-named input — §2.3).
    // outputs (inside-out): the per-iteration carry value `current_state: String`
    //   plus the implicit `self.index: Number` port (mirrors flatten_group).
    let loop_in = NodeDefinition {
        id: in_id.clone(),
        node_type: "LoopIn".to_string(),
        label: Some(format!("{} ▸ in", sm.name)),
        config: in_config,
        position: Position { x: 0.0, y: 0.0 },
        scope: vec![],
        group_boundary: Some(GroupBoundary {
            group_id: group_id.clone(),
            role: GroupBoundaryRole::In,
        }),
        inputs: vec![PortDefinition {
            name: CARRY.to_string(),
            port_type: string_ty(),
            required: false,
            description: Some("carry seed: the SM initial state".to_string()),
            configurable: string_ty().is_default_configurable(),
            synthesized_from_carry: false,
        }],
        outputs: vec![
            PortDefinition {
                name: CARRY.to_string(),
                port_type: string_ty(),
                required: false,
                description: Some("current state for this iteration".to_string()),
                configurable: string_ty().is_default_configurable(),
                synthesized_from_carry: false,
            },
            // Implicit per-iteration index port (mirrors flatten_group).
            PortDefinition {
                name: "index".to_string(),
                port_type: number_ty(),
                required: false,
                description: None,
                configurable: false,
                synthesized_from_carry: false,
            },
        ],
        features: NodeFeatures::default(),
        requires_infra: false,
        images: vec![],
        span: None,
        header_span: None,
        config_spans: Default::default(),
        file_refs: Default::default(),
        include_path: None,
    };

    // --- LoopOut boundary node. ------------------------------------------------
    // inputs (inside-in): the carry-write port `current_state: String` plus the
    //   implicit reserved `done: Boolean` port (mirrors flatten_group).
    // outputs (outer-out): the final carry value `current_state: String`. The
    //   carry validator pairs LoopOut.outputs[current_state] with
    //   LoopIn.inputs[current_state] and requires equal types — both String.
    let loop_out = NodeDefinition {
        id: out_id.clone(),
        node_type: "LoopOut".to_string(),
        label: Some(format!("{} ▸ out", sm.name)),
        config: serde_json::json!({ "parentId": group_id }),
        position: Position { x: 0.0, y: 0.0 },
        scope: vec![],
        group_boundary: Some(GroupBoundary {
            group_id: group_id.clone(),
            role: GroupBoundaryRole::Out,
        }),
        inputs: vec![
            PortDefinition {
                name: CARRY.to_string(),
                port_type: string_ty(),
                required: false,
                description: Some("carry write: the next state".to_string()),
                configurable: string_ty().is_default_configurable(),
                synthesized_from_carry: false,
            },
            PortDefinition {
                name: "done".to_string(),
                port_type: bool_ty(),
                required: false,
                description: None,
                configurable: false,
                synthesized_from_carry: false,
            },
        ],
        outputs: vec![PortDefinition {
            name: CARRY.to_string(),
            port_type: string_ty(),
            required: false,
            description: Some("final carry value".to_string()),
            configurable: string_ty().is_default_configurable(),
            synthesized_from_carry: false,
        }],
        features: NodeFeatures::default(),
        requires_infra: false,
        images: vec![],
        span: None,
        header_span: None,
        config_spans: Default::default(),
        file_refs: Default::default(),
        include_path: None,
    };

    // --- Seed node: a constant emitting `sm.initial` (§2.3). -------------------
    // A generic `Const` placeholder; the value lives in `config.value`. P6e will
    // not touch this — the carry seed is state-only, never a domain artifact.
    let seed = NodeDefinition {
        id: seed_id.clone(),
        node_type: "Const".to_string(),
        label: Some(format!("{} ▸ initial", sm.name)),
        config: serde_json::json!({ "parentId": group_id, "value": sm.initial }),
        position: Position { x: 0.0, y: 0.0 },
        scope: vec![group_id.clone()],
        group_boundary: None,
        inputs: vec![],
        outputs: vec![PortDefinition {
            name: "value".to_string(),
            port_type: string_ty(),
            required: false,
            description: None,
            configurable: false,
            synthesized_from_carry: false,
        }],
        features: NodeFeatures::default(),
        requires_infra: false,
        images: vec![],
        span: None,
        header_span: None,
        config_spans: Default::default(),
        file_refs: Default::default(),
        include_path: None,
    };

    // --- Dispatcher: a placeholder router keyed on `current_state` (§3). -------
    // Generic skeleton: no per-state subgraph is emitted here (that's P6e). It
    // takes the iteration's state and emits the produced event symbol. The doc
    // (§3) leaves the router node-type choice to P6d; we mint a generated
    // `SmDispatch` placeholder rather than reuse a Switch, because no per-state
    // subgraph exists yet to fan into.
    let dispatch = NodeDefinition {
        id: dispatch_id.clone(),
        node_type: "SmDispatch".to_string(),
        label: Some(format!("{} ▸ dispatch", sm.name)),
        config: serde_json::json!({ "parentId": group_id }),
        position: Position { x: 0.0, y: 0.0 },
        scope: vec![group_id.clone()],
        group_boundary: None,
        inputs: vec![PortDefinition {
            name: CARRY.to_string(),
            port_type: string_ty(),
            required: false,
            description: Some("state to dispatch on".to_string()),
            configurable: string_ty().is_default_configurable(),
            synthesized_from_carry: false,
        }],
        outputs: vec![PortDefinition {
            name: "event".to_string(),
            port_type: string_ty(),
            required: false,
            description: Some("the event symbol produced this iteration".to_string()),
            configurable: false,
            synthesized_from_carry: false,
        }],
        features: NodeFeatures::default(),
        requires_infra: false,
        images: vec![],
        span: None,
        header_span: None,
        config_spans: Default::default(),
        file_refs: Default::default(),
        include_path: None,
    };

    // --- Transition selector: writes carry + done (§3). -----------------------
    // Reads (current_state, event), looks up the lowered transition table, and
    // produces `to` (the next state) and `done = (to ∈ terminal)`. The lowered
    // table + terminal set are carried as config so the table is fully described
    // for P6e / runtime. The carry/done WRITES are LoopOut edges, below — the
    // binding runtime contract.
    let table: Vec<serde_json::Value> = sm
        .transitions
        .iter()
        .map(|t| {
            serde_json::json!({
                "from": t.from,
                "event": t.event,
                "to": t.to,
            })
        })
        .collect();
    let selector = NodeDefinition {
        id: selector_id.clone(),
        node_type: "SmTransitionSelector".to_string(),
        label: Some(format!("{} ▸ selector", sm.name)),
        config: serde_json::json!({
            "parentId": group_id,
            "table": table,
            "terminal": sm.terminal,
        }),
        position: Position { x: 0.0, y: 0.0 },
        scope: vec![group_id.clone()],
        group_boundary: None,
        inputs: vec![
            PortDefinition {
                name: CARRY.to_string(),
                port_type: string_ty(),
                required: false,
                description: Some("current state".to_string()),
                configurable: string_ty().is_default_configurable(),
                synthesized_from_carry: false,
            },
            PortDefinition {
                name: "event".to_string(),
                port_type: string_ty(),
                required: false,
                description: Some("event produced this iteration".to_string()),
                configurable: string_ty().is_default_configurable(),
                synthesized_from_carry: false,
            },
        ],
        outputs: vec![
            PortDefinition {
                name: "to".to_string(),
                port_type: string_ty(),
                required: false,
                description: Some("next state".to_string()),
                configurable: false,
                synthesized_from_carry: false,
            },
            PortDefinition {
                name: "done".to_string(),
                port_type: bool_ty(),
                required: false,
                description: Some("true when `to` is a terminal state".to_string()),
                configurable: false,
                synthesized_from_carry: false,
            },
        ],
        features: NodeFeatures::default(),
        requires_infra: false,
        images: vec![],
        span: None,
        header_span: None,
        config_spans: Default::default(),
        file_refs: Default::default(),
        include_path: None,
    };

    // --- Edges (§2.3 seed + §3 carry/done wiring). ----------------------------
    let edges = vec![
        // Carry seed: const(value) -> LoopIn.current_state (engine seeds carry
        // from the same-named LoopIn input on first instantiation).
        Edge {
            id: format!("{group_id}__e_seed"),
            source: seed_id.clone(),
            source_handle: Some("value".to_string()),
            target: in_id.clone(),
            target_handle: Some(CARRY.to_string()),
            span: None,
        },
        // LoopIn.current_state -> dispatch.current_state (this iteration's state).
        Edge {
            id: format!("{group_id}__e_in_dispatch"),
            source: in_id.clone(),
            source_handle: Some(CARRY.to_string()),
            target: dispatch_id.clone(),
            target_handle: Some(CARRY.to_string()),
            span: None,
        },
        // LoopIn.current_state -> selector.current_state (selector needs `from`).
        Edge {
            id: format!("{group_id}__e_in_selector"),
            source: in_id.clone(),
            source_handle: Some(CARRY.to_string()),
            target: selector_id.clone(),
            target_handle: Some(CARRY.to_string()),
            span: None,
        },
        // dispatch.event -> selector.event.
        Edge {
            id: format!("{group_id}__e_dispatch_selector"),
            source: dispatch_id.clone(),
            source_handle: Some("event".to_string()),
            target: selector_id.clone(),
            target_handle: Some("event".to_string()),
            span: None,
        },
        // selector.to -> LoopOut.current_state (self.current_state = to: carry write).
        Edge {
            id: format!("{group_id}__e_carry_write"),
            source: selector_id.clone(),
            source_handle: Some("to".to_string()),
            target: out_id.clone(),
            target_handle: Some(CARRY.to_string()),
            span: None,
        },
        // selector.done -> LoopOut.done (self.done = to ∈ terminal: done vote).
        Edge {
            id: format!("{group_id}__e_done"),
            source: selector_id.clone(),
            source_handle: Some("done".to_string()),
            target: out_id.clone(),
            target_handle: Some("done".to_string()),
            span: None,
        },
    ];

    // --- The one Loop group. ---------------------------------------------------
    let group = GroupDefinition {
        id: group_id.clone(),
        kind: GroupKind::Loop {
            loop_config: serde_json::json!({
                "parallel": false,
                "over": [],
                "carry": [CARRY],
                "max_iters": sm.max_iters,
                "trim_on_mismatch": true,
            }),
        },
        label: Some(sm.name.clone()),
        in_ports: vec![PortDefinition {
            name: CARRY.to_string(),
            port_type: string_ty(),
            required: false,
            description: None,
            configurable: string_ty().is_default_configurable(),
            synthesized_from_carry: false,
        }],
        out_ports: vec![PortDefinition {
            name: CARRY.to_string(),
            port_type: string_ty(),
            required: false,
            description: None,
            configurable: string_ty().is_default_configurable(),
            synthesized_from_carry: false,
        }],
        one_of_required: vec![],
        parent_group_id: None,
        child_group_ids: vec![],
        node_ids: vec![seed_id, dispatch_id, selector_id],
        anonymous: false,
        span: None,
        header_span: None,
    };

    (
        vec![loop_in, loop_out, seed, dispatch, selector],
        edges,
        vec![group],
    )
}
