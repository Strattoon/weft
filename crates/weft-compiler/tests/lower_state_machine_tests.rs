//! P6d: structural lowering of a `StateMachineDef` onto the durable While-loop
//! spine. These tests pin the CONTROL SKELETON (group kind, LoopConfig, LoopIn/
//! LoopOut boundary nodes, the `initial` seed, and the carry/done LoopOut wiring)
//! and the LEGALITY of the emitted loop against `check_loop_config`. NO engine
//! run, NO real domain nodes, NO tagged-flow tags (those are P6e).

use weft_compiler::lower_state_machine::lower_state_machine;
use weft_compiler::validate::{validate, ValidationMode, validate_with_mode};
use weft_compiler::Diagnostic;
use weft_core::node::{MetadataCatalog, NodeMetadata};
use weft_core::project::{GroupKind, ProjectDefinition};
use weft_core::state_machine::{SmOwner, SmTransition, StateMachineDef};

/// Empty catalog: `check_loop_config` consults no metadata, so an empty catalog
/// is sufficient to exercise the loop-config legality family.
struct EmptyCat;
impl MetadataCatalog for EmptyCat {
    fn lookup(&self, _t: &str) -> Option<&NodeMetadata> {
        None
    }
    fn all(&self) -> Vec<&NodeMetadata> {
        Vec::new()
    }
}

/// The brief's golden SM: A --e1--> B --e2--> C, terminal=[C], initial=A.
fn three_state_sm(max_iters: u32) -> StateMachineDef {
    StateMachineDef {
        name: "Demo".to_string(),
        initial: "A".to_string(),
        terminal: vec!["C".to_string()],
        authority: vec![],
        max_iters,
        transitions: vec![
            SmTransition {
                from: "A".to_string(),
                event: "e1".to_string(),
                to: "B".to_string(),
                owner: SmOwner::Script,
                guard: Some("always".to_string()),
                artifact: None,
            },
            SmTransition {
                from: "B".to_string(),
                event: "e2".to_string(),
                to: "C".to_string(),
                owner: SmOwner::Script,
                guard: Some("always".to_string()),
                artifact: None,
            },
        ],
    }
}

/// The loop-config diagnostic family: any of these codes on the lowered fragment
/// would mean we emitted an illegal While loop.
const LOOP_CONFIG_CODES: &[&str] = &[
    "loop-boundary-unpaired",
    "loop-unknown-config-field",
    "loop-parallel-not-boolean",
    "loop-config-missing-parallel",
    "loop-max-iters-not-integer",
    "loop-trim-not-boolean",
    "parallel-with-carry",
    "parallel-without-over",
    "parallel-with-done",
    "loop-unbounded-no-termination",
    "over-and-carry-overlap",
    "reserved-port-name",
    "gather-output-must-be-nullable",
    "loop-over-unknown-port",
    "over-not-a-list",
    "loop-carry-unknown-port",
    "carry-port-type-mismatch",
];

fn loop_config_diags<'a>(diags: &'a [Diagnostic]) -> Vec<&'a Diagnostic> {
    diags
        .iter()
        .filter(|d| {
            d.code
                .as_deref()
                .map(|c| LOOP_CONFIG_CODES.contains(&c))
                .unwrap_or(false)
        })
        .collect()
}

#[test]
fn golden_three_state_sm_lowers_to_one_while_loop() {
    let sm = three_state_sm(64);
    let (nodes, edges, groups) = lower_state_machine(&sm);

    // Exactly one group, and it is a Loop.
    assert_eq!(groups.len(), 1, "exactly one group emitted");
    let group = &groups[0];
    let loop_config = match &group.kind {
        GroupKind::Loop { loop_config } => loop_config,
        GroupKind::Group => panic!("group must be a Loop, got Group"),
    };

    // LoopConfig: { parallel:false, over:[], carry:["current_state"],
    // max_iters: sm.max_iters, trim_on_mismatch:true }.
    assert_eq!(
        loop_config.get("parallel"),
        Some(&serde_json::Value::Bool(false)),
        "parallel: false"
    );
    assert_eq!(
        loop_config.get("over"),
        Some(&serde_json::json!([])),
        "over: []"
    );
    assert_eq!(
        loop_config.get("carry"),
        Some(&serde_json::json!(["current_state"])),
        "carry: [current_state]"
    );
    assert_eq!(
        loop_config.get("max_iters").and_then(|v| v.as_u64()),
        Some(64),
        "max_iters mirrors sm.max_iters"
    );
    assert_eq!(
        loop_config.get("trim_on_mismatch"),
        Some(&serde_json::Value::Bool(true)),
        "trim_on_mismatch: true"
    );

    // A LoopIn and a LoopOut boundary node (real node_type strings).
    let loop_in = nodes
        .iter()
        .find(|n| n.node_type == "LoopIn")
        .expect("a LoopIn node");
    let loop_out = nodes
        .iter()
        .find(|n| n.node_type == "LoopOut")
        .expect("a LoopOut node");

    // The boundary nodes belong to the emitted group.
    let gb_in = loop_in.group_boundary.as_ref().expect("LoopIn group_boundary");
    let gb_out = loop_out.group_boundary.as_ref().expect("LoopOut group_boundary");
    assert_eq!(gb_in.group_id, group.id, "LoopIn bound to the loop group");
    assert_eq!(gb_out.group_id, group.id, "LoopOut bound to the loop group");

    // current_state carry: input on LoopIn, output on LoopOut (validator pairs them).
    assert!(
        loop_in.inputs.iter().any(|p| p.name == "current_state"),
        "LoopIn has the current_state carry input"
    );
    assert!(
        loop_out.outputs.iter().any(|p| p.name == "current_state"),
        "LoopOut has the current_state carry output"
    );
    // carry-write + done go in on LoopOut inputs.
    assert!(
        loop_out.inputs.iter().any(|p| p.name == "current_state"),
        "LoopOut has the current_state carry-write input"
    );
    assert!(
        loop_out.inputs.iter().any(|p| p.name == "done"),
        "LoopOut has the reserved done input"
    );

    // A seed node emitting "A" (sm.initial).
    let seed = nodes
        .iter()
        .find(|n| {
            n.config.get("value").and_then(|v| v.as_str()) == Some("A")
        })
        .expect("a seed node emitting the initial state \"A\"");

    // Carry seed edge: seed -> LoopIn.current_state.
    let loop_in_id = &loop_in.id;
    assert!(
        edges.iter().any(|e| {
            e.source == seed.id
                && e.target == *loop_in_id
                && e.target_handle.as_deref() == Some("current_state")
        }),
        "seed wired to LoopIn current_state carry input"
    );

    // Carry-write edge into LoopOut current_state (transition selector writes `to`).
    let loop_out_id = &loop_out.id;
    assert!(
        edges.iter().any(|e| {
            e.target == *loop_out_id && e.target_handle.as_deref() == Some("current_state")
        }),
        "a carry write into LoopOut current_state"
    );

    // done write into LoopOut done (done = to in terminal).
    assert!(
        edges.iter().any(|e| {
            e.target == *loop_out_id && e.target_handle.as_deref() == Some("done")
        }),
        "a done write into LoopOut done"
    );
}

#[test]
fn lowered_loop_is_a_legal_while_shape() {
    let sm = three_state_sm(64);
    let (nodes, edges, groups) = lower_state_machine(&sm);

    let project = ProjectDefinition {
        id: uuid::Uuid::nil(),
        nodes,
        edges,
        groups,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    let diags = validate(&project, &EmptyCat);
    let loop_diags = loop_config_diags(&diags);
    assert!(
        loop_diags.is_empty(),
        "lowered loop must produce ZERO loop-config diagnostics, got: {:?}",
        loop_diags
            .iter()
            .map(|d| (d.code.as_deref(), d.message.as_str()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn over_exhausted_cannot_preempt_termination() {
    // §2.2 to-confirm: with `over: []`, `over_exhausted = (index+1 >= iter_count)`
    // must never fire before `done`/`max_iters`. The lowering leaves `over` empty
    // (no iter_count is pinned by the lowering); the engine's LoopIn handler owns
    // iter_count for the empty-over case. We pin the legality of that decision
    // here: the emitted LoopConfig is the canonical While shape — sequential,
    // empty over, a non-empty carry, AND a wired done vote — so termination is
    // driven by done/max_iters, never by over-exhaustion.
    let sm = three_state_sm(64);
    let (nodes, edges, groups) = lower_state_machine(&sm);

    let loop_config = match &groups[0].kind {
        GroupKind::Loop { loop_config } => loop_config,
        GroupKind::Group => panic!("must be a loop"),
    };
    // While shape: parallel=false, over=[], carry non-empty.
    assert_eq!(loop_config.get("parallel"), Some(&serde_json::Value::Bool(false)));
    assert_eq!(loop_config.get("over"), Some(&serde_json::json!([])));
    assert_eq!(loop_config.get("carry"), Some(&serde_json::json!(["current_state"])));

    // A done vote IS wired (so DoneVoted termination is reachable). Find the loop
    // group's LoopOut id and assert a done edge targets it.
    let loop_out = nodes.iter().find(|n| n.node_type == "LoopOut").unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e.target == loop_out.id && e.target_handle.as_deref() == Some("done")),
        "done vote must be wired so termination is done-driven, not over-driven"
    );

    // And max_iters is the belt-and-suspenders cap.
    assert!(
        loop_config.get("max_iters").and_then(|v| v.as_u64()).is_some(),
        "max_iters cap present as the cycle guard"
    );

    // Belt: the structural-mode validate also sees zero loop-config diagnostics.
    let project = ProjectDefinition {
        id: uuid::Uuid::nil(),
        nodes,
        edges,
        groups,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let diags = validate_with_mode(&project, &[], &EmptyCat, ValidationMode::Structural);
    assert!(loop_config_diags(&diags).is_empty());
}
