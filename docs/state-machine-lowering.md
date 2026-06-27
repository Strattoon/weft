# State-Machine Surface → Loop Lowering (Design Spike)

**Status:** design spike (P6a). No code. Pins the grammar + lowering that P6b
(parse), P6c (validate), P6d (lowering), and P5.1 (node ports) build against.

**Thesis.** A `StateMachine` is **not a new executor**. It is (a) new surface
syntax, (b) a validation pass, and (c) a **lowering onto Weft's existing durable
loop spine** in `crates/weft-engine/src/loop_runtime.rs`. The loop's carry value
is the current state; the body dispatches on that state; transitions update the
carry; terminal states stop the loop. Everything durable (journal-fold resume,
crash-replay idempotency, cancellation) is inherited unchanged from the loop
runtime — the SM adds zero new persistence surface.

This spike is grounded in the real code as of branch `feat/harness-demo`. Every
lowering decision below cites the type/function/line it targets.

---

## 0. Real-code anchor map

| Concern | Real anchor | Location |
|--|--|--|
| Loop config snapshot | `LoopConfig { parallel, over, carry, max_iters, trim_on_mismatch }` | `loop_runtime.rs:50-57` |
| Config parse from LoopIn JSON | `LoopConfig::from_node_config` | `loop_runtime.rs:68-93` |
| Live instance, carry store | `LoopInstance.carry_values: HashMap<String, Value>` | `loop_runtime.rs:146` |
| Accumulated-input snapshot | `LoopInstance.outer_input: HashMap<String, Value>` | `loop_runtime.rs:151` |
| Resume rebuild | `LoopInstance::from_snapshot` | `loop_runtime.rs:185-213` |
| Advance verdict | `LoopAdvance::{LaunchNext{index}, EmitOutward{reason,gather,carry}, Idle}` | `loop_runtime.rs:218-235` |
| Lookup/instantiate | `LoopRuntime::ensure` | `loop_runtime.rs:261-274` |
| Record one iteration | `LoopRuntime::record_loop_out(key,index,gather,carry,done_vote)` | `loop_runtime.rs:329-427` |
| Idempotency gate | `LoopRuntime::loop_out_is_new` | `loop_runtime.rs:306-315` |
| Done-vote termination | `done` branch → `LoopTerminationReason::DoneVoted` | `loop_runtime.rs:407-409` |
| max_iters termination | `max_reached` branch | `loop_runtime.rs:381,410-412` |
| Snapshot view | `LoopInstanceSnapshot` (carry/over/outer_input) | `from_snapshot`, `loop_runtime.rs:194-211` |
| Engine LoopIn call site | `loop_runtime.ensure(...)` | `execution_driver.rs:2000` |
| Engine LoopOut call site | `loop_runtime.record_loop_out(...)` + journal gate | `execution_driver.rs:2248-2267` |
| Resume fold | `from_snapshot` rebuild loop | `execution_driver.rs:443-480` |
| Tagged-flow schema | `PortDef.produces_tags` / `PortDef.forbids_tags` (`Vec<String>`, camelCase serde alias) | `node.rs:486-492` |
| Tagged-flow pass | `check_tagged_flow` → `tagged-flow-violation` | `validate.rs:1103-1145` |
| Loop-config validator | `check_loop_config` (`gather-output-must-be-nullable`, `loop-unbounded-no-termination`) | `validate.rs:770-1004` |
| HumanQuery durable suspend | `catalog/human/query/metadata.json` (`hasFormSchema`, `await_form`) | metadata.json |
| Base shapes (While) | `docs/loops.md` §"The five base shapes" | loops.md:131-139 |
| Workday source `Transition` | `(from,event,to,owner,guard,artifact_required)` | `workday-orchestrator/.../lifecycle/transitions.rs:24-34` |
| Workday authority states | `MechanicalGreen, RouteSelected, ArtifactValidated, ProductionActionApplied` | `transitions.rs:6-7`, `states.rs:16-58` |

---

## 1. The `StateMachine` / `Transition` surface grammar (CST → AST)

### 1.1 Design intent

A `StateMachine` block is the same *family* of block as `Loop`/`Group`: a header
keyword, a name, and a brace body that mixes scalar config fields (`key: value`)
with a list section. P6b can target the exact rowan CST patterns the `Loop` block
already uses (`docs/loops.md` §Syntax — config fields like `parallel: false`,
`over: ["images"]`, `max_iters: 100`, parsed as `key: value` body lines). The new
piece is the `transitions:` list whose elements are **transition rows**, not
ordinary `key: value` pairs.

### 1.2 Concrete grammar (EBNF-ish, parseable)

```
state_machine   = "StateMachine" IDENT "{" sm_body "}" ;

sm_body         = sm_field* "transitions" ":" "[" transition_row+ "]" sm_field* ;

sm_field        = "initial"   ":" IDENT
                | "terminal"  ":" "[" ident_list "]"
                | "max_iters" ":" INT ;          # cycle-guard cap (required, see §2)

ident_list      = IDENT ( "," IDENT )* ","? ;

transition_row  = "(" IDENT "," IDENT ")" "->" IDENT
                  kv_attr* ;                      # whitespace-separated attrs

kv_attr         = "owner"    "=" owner_kw
                | "guard"    "=" IDENT
                | "artifact" "=" IDENT ;

owner_kw        = "model" | "script" | "human" | "gateway" ;
```

Reading a row `(From, Event) -> To owner=… guard=… artifact=…`:

- `From` = source state (an `IDENT` that must appear in the state set).
- `Event` = the event symbol that selects this edge out of `From`.
- `To` = target state.
- `owner` = disposing authority (closed vocabulary, 4 values).
- `guard` = named predicate label (an `IDENT`; bodies live in P6c/runtime, exactly
  like Workday's `guard: &'static str` label in `transitions.rs:30-31`).
- `artifact` = optional required artifact name on the target state.

`owner` is **required** per row (mirrors Workday — every `Transition` has an
`owner`). `guard` is **required and never empty** (mirrors the
`transitions.rs:30-31` doc-comment "never empty" and the test
`transitions_table_is_nonempty` at `transitions.rs:120-126`). `artifact` is
optional (`Option<&'static str>` in the source — `transitions.rs:33`).

### 1.3 Worked example (the brief's `RunLifecycle`)

```text
StateMachine RunLifecycle {
  initial: HumanRequestCaptured
  terminal: [ Frozen, Void, Escalated ]
  max_iters: 64
  transitions: [
    (HumanRequestCaptured, SpecDraftEmitted)     -> SpecDrafted            owner=model   guard=request_nonempty        artifact=human_spec_record
    (SpecDrafted,          SpecValidationPassed)  -> SpecValidated          owner=script  guard=required_fields_present
    (HumanApprovalRequired, ApprovalGranted)      -> ProductionActionReady  owner=human   guard=human_approved
  ]
}
```

### 1.4 AST shape (target for P6b)

```
SmDecl {
  name: String,
  initial: StateId,                 // IDENT
  terminal: Vec<StateId>,           // non-empty; subset of state set
  max_iters: u32,                   // cycle-guard cap
  transitions: Vec<SmTransition>,
}

SmTransition {                      // mirrors Workday's Transition 1:1 (§3)
  from: StateId,
  event: EventId,
  to: StateId,
  owner: SmOwner,                   // Model | Script | Human | Gateway
  guard: GuardLabel,                // non-empty IDENT
  artifact: Option<ArtifactId>,
}

SmOwner = Model | Script | Human | Gateway
```

The **state set** and **event set** are *derived* (not declared): the union of all
`from`/`to` is the state set, the union of all `event` is the event set. `initial`
and every `terminal` entry must be in the derived state set (a P6c check). This
matches Workday, where `RunState`/`RunEvent` are enums and `TRANSITIONS` is the
table over them — but in surface syntax we infer the enums from the table rather
than forcing a separate declaration.

---

## 2. Lowering to a **sequential While loop**

The SM is inherently sequential: exactly one state is active at a time, and the
next state depends on the current one. That is precisely the **While** base shape
in `docs/loops.md:138`:

| Shape | `parallel` | `over` | `carry` | Terminated by |
|--|--|--|--|--|
| While | `false` | `[]` | `[acc?]` | `self.done = true` |

### 2.1 Exact `LoopConfig` the SM lowers to

The lowering emits a `Loop` whose `LoopIn` config JSON (consumed by
`LoopConfig::from_node_config`, `loop_runtime.rs:68-93`) is:

```json
{
  "parallel": false,
  "over": [],
  "carry": ["current_state"],
  "max_iters": 64,
  "trim_on_mismatch": true
}
```

- `parallel: false` — sequential; the body sees one state per iteration. Required:
  carry + `self.done` only work in sequential mode (`docs/loops.md:138`;
  `check_loop_config` rejects `parallel: true` with carry as `parallel-with-carry`,
  `validate.rs:882-885`).
- `over: []` — the SM does not iterate a list; it loops on state. An empty `over`
  in sequential mode is exactly the While shape (`loops.md:138`).
- `carry: ["current_state"]` — the carry port **is** the state register. Its value
  is the current `RunState`; updated each iteration by the transition selector.
  The compiler may add more carry ports for accumulated artifacts (§2.4).
- `max_iters: <cycle-guard cap>` — the SM's cycle-guard. This is **mandatory** in
  the lowering, not optional. A sequential loop with empty `over`, no `max_iters`,
  and no `self.done` write is rejected by `check_loop_config` as
  `loop-unbounded-no-termination` (`validate.rs:910-922`). We satisfy that rule by
  BOTH setting `max_iters` AND wiring `self.done` (see §3) — belt and suspenders:
  `self.done` fires on a clean terminal state, `max_iters` is the guard against a
  cyclic graph (e.g. `MechanicalRed → RepairDrafted → … → MechanicalRed`) that
  never reaches a declared terminal.

### 2.2 Termination semantics inherited from the runtime

`record_loop_out` (`loop_runtime.rs:329-427`) gives the SM two independent stop
conditions for free:

1. **DoneVoted** (`loop_runtime.rs:407-409`): when the body writes
   `self.done = true` (because the new state ∈ `terminal`), the runtime terminates
   with `LoopTerminationReason::DoneVoted` and emits outward. This is the *normal*
   stop (e.g. reaching `Frozen`).
2. **MaxItersReached** (`loop_runtime.rs:381, 410-412`): the cycle guard. If the
   transition graph cycles and never votes done, the loop stops at `max_iters`. The
   precedence comment at `loop_runtime.rs:377-380` means that when both fire on the
   same iteration, `MaxItersReached` is reported — the binding constraint, correct
   for an SM that hit its guard.

`over_exhausted` (`loop_runtime.rs:382,413-415`) is unreachable for the SM:
`over: []` ⇒ `iter_count == 0`-path is not taken because carry/done drive it. (The
zero-iter path is handled in the engine's LoopIn handler, not relevant here since
the SM always launches at least the `initial` iteration.)

### 2.3 The carry seed = `initial`

On first instantiation, the engine seeds carry from the loop's same-named input
(`execution_driver.rs:1971-1982`: "Initial carry seeds come from the loop's
same-named inputs"). The lowering wires `current_state`'s LoopIn input from a
constant node emitting `initial` (`"HumanRequestCaptured"`). So iteration 0 sees
`self.current_state == initial`.

### 2.4 Accumulated artifacts

Artifacts produced along the path (e.g. `human_spec_record`, `plan_ir`) are
threaded as **additional carry ports** — one carry port per artifact name that any
transition's target requires, typed `T | Null` on the gather side if also emitted
outward. Because `outer_input` is snapshotted (`loop_runtime.rs:151`,
`from_snapshot` `loop_runtime.rs:210`), and carry survives resume
(`carry_values`, `from_snapshot` `loop_runtime.rs:209`), accumulated artifacts are
durable across a crash with no extra machinery (see §7).

---

## 3. Dispatch-group body shape

The loop body is a **router + per-state subgraph + transition-selector**, all
inside the `Loop` body scope (the body is a normal Weft sub-graph addressed by
frame stacks — `docs/loops.md:156-171`).

```
Loop body (per iteration, reads self.current_state):

  router = Router on self.current_state           # selects the active state's subgraph
      ├─ state == HumanRequestCaptured → subgraph_HRC
      ├─ state == SpecDrafted          → subgraph_SD
      └─ … (one branch per non-terminal state)

  <subgraph for the selected state>:
      - runs the state's work (a Model node, a Script node, a HumanQuery, …)
      - emits the event symbol + any produced artifact

  selector = TransitionSelector(current_state, event)
      - looks up the (from,event) row in the lowered transition table
      - computes `to`
      - self.current_state = to                    # carry update (loop_runtime.rs:2220-2224 routes carry writes)
      - self.<artifact>     = produced artifact     # if the row declares one (carry)
      - self.done = (to ∈ terminal)                 # terminal → vote done
```

Concretely against the runtime:

- `self.current_state = to` lowers to an edge into `{loop}__out` on the
  `current_state` carry port. The LoopOut firing handler routes it to
  `carry_writes` because `inst_config.carry.contains(name)`
  (`execution_driver.rs:2220-2221`), and `record_loop_out` applies it
  (`loop_runtime.rs:369-372`).
- `self.done = (to ∈ terminal)` lowers to an edge into `{loop}__out` on the
  reserved `done` port. The handler reads it as `done_vote`
  (`execution_driver.rs:2170-2200`) and `record_loop_out` terminates with
  `DoneVoted` when it is `true` (`loop_runtime.rs:407-409`). This wiring also
  satisfies the `loop-unbounded-no-termination` rule (`done_wired == true`,
  `validate.rs:899-901,915`).
- When no row matches `(current_state, event)`, the selector is a P6c error
  (analogous to Workday having no `Transition` row). At runtime an unmatched
  event is a hard failure (a stuck SM is corruption, never a silent no-op).

The router/selector are themselves lowered to existing catalog/control nodes (a
condition fan-out + a table lookup); P6d decides whether to reuse an existing
`Switch`/`Match` node or emit a small generated dispatcher. This spike does not
constrain that choice — it only fixes that the carry/done wiring above is the
runtime contract.

---

## 4. `owner` → tagged-flow tags

This is the model-proposes / script-disposes guarantee, enforced **at compile
time** through the MERGED tagged-flow schema — NOT a hypothetical one.

### 4.1 The real schema

`PortDef` (`node.rs:475-493`) carries:

```rust
pub produces_tags: Vec<String>,   // node.rs:488   (serde alias "producesTags")
pub forbids_tags:  Vec<String>,   // node.rs:492   (serde alias "forbidsTags")
```

and `check_tagged_flow` (`validate.rs:1103-1145`) emits one
`tagged-flow-violation` diagnostic per edge where a produced tag is in the target
port's `forbids_tags` (`validate.rs:1130-1142`).

### 4.2 Lowering rule

- **`owner=model`**: the transition's disposing output port (the port that carries
  the model's proposal — the emitted artifact / event) gets
  `produces_tags = ["proposal"]`.
- **Authority state input port**: the input port of any *authority* state's
  subgraph gets `forbids_tags = ["proposal"]`.

Therefore a `Model`-owned transition INTO an authority state lowers to an edge
whose source produces `"proposal"` into a port that forbids `"proposal"` →
`check_tagged_flow` emits `tagged-flow-violation` at compile time
(`validate.rs:1130-1142`). The doctrine is unprovable to violate silently: it is a
compile error, exactly like the `tagged_flow_tests::control_tag_into_forbidding_port_is_rejected`
test at `validate.rs:1346-1352`.

### 4.3 Which states are authority states

From Workday (`transitions.rs:6-7` and the invariant the B2 tests assert): the
authority states that must **never** be `Model`-owned are
`MechanicalGreen`, `RouteSelected`, `ArtifactValidated`, `ProductionActionApplied`.
The lowering tags the input ports of the subgraphs for these four states with
`forbids_tags = ["proposal"]`. The Workday source already enforces this for its
hand-rolled table — e.g. extraction INTO `ArtifactExtracted` is `Script`-owned and
guarded by the test `executor_artifact_extraction_is_script_owned`
(`transitions.rs:128-145`); the SM lowering reproduces that as a tagged-flow
edge constraint rather than a Rust test.

The canonical tag string is `"proposal"` (snake_case canonical per the
schema doc-comment at `node.rs:486-492`). `owner=script|human|gateway` produce no
`proposal` tag, so disposing transitions are always permitted into authority
states.

---

## 5. `guard` and `artifact` lowering

### 5.1 `guard` → a named predicate gate

Each row's `guard` (an `IDENT` label) lowers to a **Gate-like predicate node**
wired in series before the transition fires: the transition's event is only
accepted if the guard node passes. This mirrors Workday exactly — `guard:
&'static str` (`transitions.rs:30-31`) is a *label* whose body is implemented
separately (B2). In the lowering:

- the guard label names a predicate node in the state's subgraph;
- its boolean output is wired into the transition selector as a precondition;
- a failed guard means the `(from,event)` edge does not fire (the SM stays in
  `from` or routes to the guard-fail edge if one is declared — e.g. Workday's
  `SpecValidationFailed → HumanApprovalRequired`, `transitions.rs:57`).

Guards are predicate-only (closed-grammar-friendly); they do not dispose state by
themselves, so they carry no authority tag.

### 5.2 `artifact` → a required typed input on the target state

`artifact=<name>` lowers to a **required typed input port on the target state's
subgraph** — i.e. the target subgraph declares that port `required: true`
(`PortDef.required`, `node.rs:478-479`). This reuses the existing required-port
coverage check `required-port-unmet` (`validate.rs:618-629`): a transition that
declares `artifact=plan_ir` but reaches a target whose subgraph has no driver for
`plan_ir` is a compile error. The artifact itself is threaded as a carry port
(§2.4) so it is available to the target iteration.

Mapping to Workday: `artifact_required: Option<&'static str>` (`transitions.rs:33`)
→ `artifact: Option<ArtifactId>` in the AST → a required input on the `to` state.

---

## 6. Human-owned transitions → `HumanQuery` durable suspend

`owner=human` transitions lower to the existing `HumanQuery` node
(`catalog/human/query/metadata.json`), confirmed present with `mod.rs` and
`deps.toml`. Its surface:

- **inputs**: `context: String` (`required: false`) — the prompt/context shown to
  the reviewer.
- **outputs**: none statically; the form's outputs are **materialized from
  `fields`** via `hasFormSchema: true` (the form_builder field) and
  `form_field_specs` (`node.rs:368-408` describes the FormFieldSpec derivation).
  An `approve_reject` field, for instance, materializes the decision output port.
- **suspend mechanism**: the description states it "Pause[s] execution and wait[s]
  for a human to submit a form. Uses the `await_form` language primitive." That is
  the durable suspend — the run parks until the human responds, surviving across
  restarts via the journal.

Lowering: the Human-owned transition's subgraph is a `HumanQuery` whose
`context` is fed the state context, and whose materialized decision output drives
the event symbol (e.g. `HumanApproved` vs `HumanRejected`, matching Workday's two
human rows `transitions.rs:107-108`). The guard (`human_approved`) gates on the
decision output.

**Freeze terminal**: per the brief and Workday's table, the `Frozen` terminal is
reached *after* `ProvenanceRecorded` (`transitions.rs:110`:
`(ProvenanceRecorded, Froze) -> Frozen`). The Human approval row
`(HumanApprovalRequired, HumanApproved) -> ProvenanceRecorded`
(`transitions.rs:107`) is the durable-suspend point; once provenance is recorded,
the script-owned `Froze` transition votes `self.done = true` and the loop emits
outward with `DoneVoted`.

---

## 7. Journal-fold resume story

The SM inherits durable resume **entirely** from the loop runtime; it adds nothing.

- **Carry = state survives a crash.** On resume the engine rebuilds each
  `LoopInstance` via `LoopInstance::from_snapshot` (`loop_runtime.rs:185-213`,
  driven from `execution_driver.rs:443-480`). `from_snapshot` restores
  `carry_values` from the `LoopInstanceSnapshot` (`loop_runtime.rs:209`:
  `carry_values: snap.carry_values.clone()`). Since `current_state` is a carry
  port, **the SM's current state is reconstructed from the journal fold** — a
  mid-run crash resumes at the exact state the SM was in.
- **Accumulated artifacts survive.** `outer_input` is snapshotted and restored
  (`loop_runtime.rs:151` field; `loop_runtime.rs:210`
  `outer_input: snap.outer_input.clone()`), and the artifact carry ports (§2.4)
  ride `carry_values`. No artifact recomputation on resume.
- **Crash-replay is idempotent.** The `loop_out_is_new` gate
  (`loop_runtime.rs:306-315`, called at `execution_driver.rs:2248`) ensures a
  re-fired `LoopOut` is journaled at most once; `record_loop_out`'s replay guard
  (`loop_runtime.rs:363-374`) applies writes only on first firing and the
  `launched.contains(&next)` guard (`loop_runtime.rs:417-425`) refuses to
  re-launch an already-launched next iteration. For the SM this means a crash
  between "wrote new state" and "launched next iteration" cannot double-advance the
  state machine.
- **Human suspend resume.** A run parked in a `HumanQuery` (`await_form`) is
  durable for the same reason — the carry (current state =
  `HumanApprovalRequired`) is in the snapshot, so a restart re-enters the suspended
  human step rather than restarting the SM.

No new journal events, no new snapshot fields: the SM is a loop, and the loop's
fold already covers it.

---

## 8. Open risks (explicitly deferred)

1. **Nested state machines.** An SM-as-state (a sub-SM inside one state's
   subgraph) would lower to a nested loop. The runtime supports nested loops
   (cancellation cascade tests at `loop_runtime.rs:726-749` exercise nested
   instances), but the *grammar* and the *tag-propagation across the nesting
   boundary* are out of scope here. Deferred.
2. **Parallel / concurrent transitions.** The While shape is strictly
   sequential (one state at a time). Fork/join states (two transitions firing
   concurrently) have no lowering target — `parallel: true` forbids carry and
   `self.done` (`validate.rs:882-908`), which the SM needs. Deferred; would
   require a different lowering (likely a parallel sub-loop per branch with a join
   state).
3. **Large transition tables in one loop body.** Every non-terminal state's
   subgraph lives in a single loop body scope. A very large table (Workday's full
   table is ~35 rows / ~30 states) produces a large dispatch body. Whether this
   compiles/renders acceptably, and whether the router should be a generated table
   node vs. an N-way `Switch`, is a P6d performance/ergonomics question. Deferred.
4. **Self-loops and guard-fail back-edges.** Rows like Workday's repair cycle
   (`MechanicalRed → RepairDrafted → … → MechanicalRed`) rely on `max_iters` as
   the only guard. Tuning the cycle-guard cap (and surfacing "hit the guard" vs.
   "reached terminal" to the operator) is left to the runtime story; the
   `MaxItersReached` reason is already distinct (`loop_runtime.rs:381`).
5. **Event-set / state-set inference ambiguity.** Deriving the state and event
   sets from the table (§1.4) means a typo in a state name silently introduces a
   new unreachable state instead of erroring against a declared enum (Workday gets
   this for free via Rust enums). P6c must add an `unreachable-state` /
   `undeclared-terminal` check. Flagged, not yet designed.

---

## Appendix A: Workday `Transition` field coverage (Step 3)

Every field of Workday's `Transition` struct
(`workday-orchestrator/src/lifecycle/transitions.rs:24-34`) has a lowering target.

| Workday `Transition` field | Source ref | SM grammar | Lowering target |
|--|--|--|--|
| `from: RunState` | `transitions.rs:26` | row LHS `From` | router branch key (`self.current_state == From`), §3 |
| `event: RunEvent` | `transitions.rs:27` | row LHS `Event` | event symbol the subgraph emits; selector lookup key, §3 |
| `to: RunState` | `transitions.rs:28` | row RHS `To` | `self.current_state = To` carry write, §3 (`loop_runtime.rs:369-372`) |
| `owner: TransitionOwner` | `transitions.rs:29` | `owner=` attr | tagged-flow tags: `model`→`produces_tags=["proposal"]`, authority input `forbids_tags=["proposal"]`, §4 (`node.rs:486-492`, `validate.rs:1103-1145`) |
| `guard: &'static str` | `transitions.rs:30-31` | `guard=` attr | named predicate gate node, precondition on the transition, §5.1 |
| `artifact_required: Option<&'static str>` | `transitions.rs:33` | `artifact=` attr (optional) | required typed input on `to` state's subgraph, §5.2 (`PortDef.required` `node.rs:478`, `required-port-unmet` `validate.rs:618-629`) |

**Gaps found:** none at the field level — all six `Transition` fields lower.

Two **structural** items Workday expresses outside the per-row struct that the SM
grammar must also carry, and does:

- **Terminal set.** Workday encodes terminals implicitly (`Frozen`, `Void`,
  `Escalated` are `RunState` variants with no outgoing rows — `states.rs:54-57`).
  The SM grammar makes this **explicit** via `terminal: [...]` (§1.2). This is an
  intentional improvement, not a gap: explicit terminals let the selector compute
  `self.done` (§3) without a reachability analysis. P6c should additionally warn if
  a declared terminal has outgoing rows, or a non-terminal state has no outgoing
  rows (a dead end that is not a declared terminal) — flagged in §8 risk 5.
- **`TransitionOwner::Gateway`.** Workday has a fourth owner, `Gateway`
  (`transitions.rs:16-21`), used on production-tail rows (e.g.
  `transitions.rs:104,106,111`). The grammar includes `owner=gateway`
  (`owner_kw`, §1.2). Gateway disposes (like Script), so it produces no `proposal`
  tag and is permitted into authority states — same tag treatment as `script`.
  **Not a gap**, but called out because the brief's worked example only showed
  three owners; the lowering covers all four.

One **observation** (not a gap): Workday's `guard` is *always present and never
empty* (`transitions.rs:30-31` + test `transitions.rs:120-126`), so the SM grammar
makes `guard=` required per row. If a future SM wants an unconditional transition,
it uses the sentinel `guard=always` (Workday's own convention, e.g.
`transitions.rs:59,86,94`), keeping the field non-empty.
