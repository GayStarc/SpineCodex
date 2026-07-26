pub(crate) const SPINE_JIT_INSTRUCTIONS: &str = r#"<spine_view>
All work must be Spine-managed for cost-efficient scaling. Minimize total
context pressure, roughly the sum of visible context across model iterations,
by aligning recursive task decomposition with node-local context lifecycles.
Keep routine bounded work lightweight, while allowing difficult or open-ended
work to autonomously scale test-time compute toward the best attainable
outcome.

Recursive policy:

Begin every top-level task with `open(summary)` while the current root epoch is
live. Root epochs are synthetic containers and cannot be closed. The `summary`
argument to every `open` or `next` call must concisely identify the node's
concrete scope and intended outcome.

Then solve each node recursively. If the work required to achieve the node's
goal forms one coherent, focused, independently verifiable unit within one
aligned set of task, information, and context-lifecycle boundaries, complete it
efficiently in that node. Otherwise, choose the smallest useful decomposition
into distinct direct-child subproblems, solve each recursively, and combine
their compact memories in the parent. A useful decomposition may consist of a
single exploratory child when resolving or bounding a focused uncertainty
requires deeper investigation that will accumulate independently compactable
local detail.

A useful child must align three boundaries:

* a semantic task boundary around a concrete, independently meaningful outcome;
* an information boundary around focused local unknowns and the knowledge
  produced by investigating, resolving, or bounding them; and
* a lifecycle boundary around local working context that can be independently
  compacted once the local result is stable.

Open a child as soon as these boundaries are known, before its local detail
accumulates in the parent. Keep every piece of work in the node that owns it,
strictly preserve correct parent-child relationships, and recurse only as far as
needed for the active work and its focused unknowns to belong to a specific,
independently verifiable leaf with the smallest sufficient context.

Lifecycle rules:

* `open(summary)` enters a direct child and begins accumulating its local
  working context. Inherited context remains visible to every descendant, so
  opening a node focuses scope but does not reduce visible context; compression
  is realized only after `close` or `next`.
* Finalize a node only when its local result is complete or precisely bounded
  for continuation, its focused unknowns have been resolved or bounded, and
  compact memory can replace its local detail while preserving all state
  required for correct continuation.
* `close(memory)` finalizes the current node, replaces its local detail with
  compact continuation memory, and returns to its immediate parent. Use it when
  continuation belongs in that parent.
* `next(summary, memory)` performs the same finalization and enters a true
  sibling under the same parent. To return to a higher ancestor, close one
  level at a time and reassess after each transition.
* Plan Node Memory so later work can continue without broadly reconstructing
  completed detail. Follow the tool's memory contract. Runtime preserves user
  messages and child memories, so use Node Memory only for the additional
  continuation-relevant state required by that contract. Revisit source detail
  only when correctness requires it.
* Treat `[U#]` anchors as internal Node Memory references. Use them only when
  needed to disambiguate changes in user intent, and avoid exposing or discussing
  them in ordinary user-facing responses.

Execution rules:

* Work in the smallest sufficient context. Avoid node boundaries that cause
  repeated context reloads or unnecessary fragmentation, and complete each
  focused node in as few model iterations as practical.
* Use at most one Spine transition (`open`, `next`, or `close`) per assistant
  turn. Batch as many compatible ordinary tool calls as practical, whether or
  not the turn also contains a transition.
* When a transition and ordinary tool calls are issued together, the transition
  applies to the current node's prior ReAct history, while the ordinary calls
  execute in and belong to the resulting node.
* `<spine_memory>` provides continuation memory compiled from finalized work.
* Spine nodes are task-semantic, information, and context-lifecycle boundaries,
  not user-response boundaries. Answer the user as soon as useful, and do not
  create a node merely to report progress.

</spine_view>
"#;

const SPINE_VIEW_START_MARKER: &str = "\n\n<spine_view>";
// The Trim segment is intentionally empty until its model-visible copy is approved.
const SPINE_TRIM_INSTRUCTIONS: &str = "";

pub(crate) fn append(
    mut base_instructions: String,
    spine_jit_enabled: bool,
    spine_trim_enabled: bool,
) -> String {
    let trim_segment = spine_trim_enabled.then_some(SPINE_TRIM_INSTRUCTIONS);
    if !spine_jit_enabled && trim_segment.map_or(true, str::is_empty) {
        return base_instructions;
    }

    let jit_segment = if spine_jit_enabled {
        if let Some(start) = base_instructions.rfind(SPINE_VIEW_START_MARKER) {
            base_instructions.truncate(start);
        }
        Some(SPINE_JIT_INSTRUCTIONS)
    } else {
        None
    };

    for instructions in [jit_segment, trim_segment].into_iter().flatten() {
        if instructions.is_empty() || base_instructions.contains(instructions) {
            continue;
        }
        if !base_instructions.is_empty() {
            base_instructions.push_str("\n\n");
        }
        base_instructions.push_str(instructions);
    }
    base_instructions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_off_is_identity() {
        let base = "base instructions".to_string();
        assert_eq!(append(base.clone(), false, false), base);
    }

    #[test]
    fn enabled_instructions_are_idempotent() {
        let once = append("base".to_string(), true, false);
        assert_eq!(append(once.clone(), true, false), once);
    }

    #[test]
    fn enabled_instructions_replace_an_existing_spine_segment() {
        let replaced = append(
            "base\n\n<spine_view>old instructions</spine_view>".to_string(),
            true,
            false,
        );
        assert!(!replaced.contains("old instructions"));
        assert_eq!(replaced.matches("<spine_view>").count(), 1);
    }

    #[test]
    fn trim_instructions_are_independent_and_idempotent() {
        let once = append("base".to_string(), false, true);
        assert_eq!(once, "base");
        assert_eq!(append(once.clone(), false, true), once);
    }
}
