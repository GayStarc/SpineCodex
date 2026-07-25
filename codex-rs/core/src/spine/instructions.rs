pub(crate) const SPINE_JIT_INSTRUCTIONS: &str = r#"<spine_view>
All work must be Spine-managed. The Spine tree enables cost-efficient scaling
by combining recursive task decomposition with node-local context lifecycle
management: each scoped node focuses work and accumulates local detail, then
replaces that detail with compact continuation memory when its local result is
stable. Routine bounded tasks remain lightweight, while difficult or
open-ended tasks can autonomously scale test-time compute toward the best
attainable outcome.

Proactively plan and decompose tasks into nodes while managing each node's
local context lifecycle, and strictly preserve correct parent-child
relationships. Complete each node's work efficiently within its scope.
Treat the Spine tree as the task's semantic scope hierarchy. Each node defines
three aligned boundaries: a semantic task boundary; an information boundary
around focused local unknowns and the knowledge produced by exploring them; and
a lifecycle boundary for its node-local working context. `open` enters such a
local scope as a direct child and begins accumulating the detailed context
needed for its work; inherited context remains visible. `close` means the local
work has reached a stable local result, completed or precisely bounded for
continuation: its focused unknowns have become known or bounded, and its
detailed context can be replaced by compact continuation memory so later work
does not need to broadly reconstruct it. It then returns to the immediate
parent. Each piece of work belongs in the node that owns it, and `next` closes
the current scope and enters a sibling under the same parent.
Use `$spine-plan-seed` when long-running work benefits from a durable plan.

Core workflow:

1. Begin a new top-level task with
   `open(<concrete, appropriately scoped task goal>)` while the current root
   epoch is active.
2. If the current node contains multiple pieces of work with distinct task,
   information, and context-lifecycle boundaries, or if one part requires
   deeper exploration whose focused unknowns are expected to produce
   independently compactable detail, use
   `open(<concrete, appropriately scoped direct-child goal>)` to enter one such
   piece. Apply this recursively as needed until the active work and its focused
   unknowns belong in a focused, specific, and verifiable leaf with the smallest
   sufficient context.
3. Use `next(<concrete sibling goal>, memory)` when the next work is a true
   sibling under the same parent.
4. Use `close(memory)` when the current node's local work has reached a stable
   local result, completed or precisely bounded, and its detailed context can
   be replaced by memory for correct continuation, and the next work belongs to
   its immediate parent. To return to a higher ancestor, close one level at a
   time and reassess after each transition.

Conventions:

* Minimize total context pressure, roughly the sum of visible context across
  model iterations, by aligning recursive task decomposition with independently
  compactable context lifecycles, batching compatible local actions, and
  completing each focused node in as few iterations as practical. When a
  distinct subproblem has focused local unknowns whose detailed context is
  expected to become independently compactable after their resolution, open
  that node as soon as this task, information, and context-lifecycle boundary is
  known, before accumulating that detail in the parent. `open` focuses scope but
  does not reduce inherited context; inherited visible context remains available
  to every descendant, and the boundary's compression benefit is realized only
  after `close` or `next`. Plan boundaries and Node Memory so later nodes can
  continue from compact decisions, results, and remaining obligations instead
  of broadly reloading or reconstructing the same detailed context; revisit
  source details only when correctness requires it.
* Work on each subtask in the smallest sufficient context: open and close nodes
  at boundaries that keep the active context focused without causing repeated
  context reloads. Use the context pressure reported in `<spine_status>` to
  adjust these boundaries and reduce total task cost.
* Use at most one Spine transition per assistant turn. Ordinary task tools may
  accompany it and belong to the resulting node; the transition applies to the
  current node's prior ReAct history.
* After `close` or `next`, `memory` replaces the finalized node's local working
  content; follow the tool parameter description to preserve the state required
  for continuation. Runtime preserves user messages and child memories, so use
  Node Memory for the additional continuation state they do not already
  provide.
* Root epochs are synthetic containers and cannot be closed.
* `<spine_status>` provides current-node orientation. `<spine_memory>` provides
  continuation memory compiled from finalized work.
* Spine nodes define task-semantic and context-lifecycle boundaries rather than
  user-response boundaries, so answer the user as soon as useful and create
  nodes only for work that needs its own task scope and local context lifecycle.

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
