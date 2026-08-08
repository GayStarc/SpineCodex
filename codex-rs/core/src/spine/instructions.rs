pub(crate) const SPINE_JIT_INSTRUCTIONS: &str = r#"<spine_view>
All work must be Spine-managed. Structure the tree around context ownership and
lifecycle. Keep each body of working context in the lowest node whose scope
spans all work that needs its exact detail. Once remaining work can continue
from compact continuation memory, let that memory replace the full detail.
Keep routine bounded work lightweight, while allowing difficult or open-ended
work to autonomously scale test-time compute toward the best attainable
outcome.

Recursive policy:

Root epochs are synthetic containers and cannot be closed. The `summary`
argument to every `open` or `next` call must concisely identify the node's
concrete scope and intended outcome.

Then solve each node recursively. Derive node boundaries from context ownership
and lifecycle. Keep work in one node only while its required working context
shares a common ownership scope and lifecycle. If achieving one outcome spans
multiple independently compactable bodies of local context, decompose the
associated work along those ownership and lifecycle boundaries into direct
children, even when all of it serves the same semantic outcome.

A useful child owns a concrete, independently meaningful body of work and the
local working context needed to complete it. That context must have an
independent lifecycle: its exact detail can become unnecessary to remaining
work once the child's result is stable and its compact memory preserves the
state required for continuation. A useful decomposition may consist of a single
exploratory child when resolving or bounding a focused uncertainty will
accumulate such independently compactable local detail.

Keep the minimum context whose exact detail is needed by multiple branches in
their lowest common ancestor for as long as those branches need it. Keep context
needed by only one branch in the child that owns that work. A child boundary is
useful only when compact memory lets remaining work continue without broadly
reconstructing the child's working context. Avoid node boundaries that cause
repeated reloads of unchanged working context or fragment one ownership and
lifecycle scope without enabling independent compaction.

When decomposing, choose the smallest useful set of direct children, solve each
recursively, and continue in the parent from their compact memories. Open a
child as soon as its context ownership and lifecycle are clear, before its local
detail accumulates in the parent. Strictly preserve correct parent-child
relationships, and recurse only until the active work and its working context
have a clear owner in a focused leaf.

Lifecycle rules:

* `open(summary)` enters a direct child and begins the lifecycle of the working
  context it owns. Inherited context remains visible to every descendant, so
  opening a node focuses ownership but does not reduce visible context;
  compression is realized only after `close` or `next`.
* Finalize a node only when its owned work is complete or precisely bounded,
  its result is stable, and continuation no longer needs its full working
  context because compact memory preserves all required state.
* `close(memory)` finalizes the current node, replaces its working context with
  compact continuation memory, and returns to its immediate parent. Use it when
  the remaining work and context belong in that parent.
* `next(summary, memory)` performs the same finalization and enters a true
  sibling under the same parent. To return to a higher ancestor, close one
  level at a time and reassess after each transition.
* Follow the tool's Node Memory contract. Runtime preserves user messages and
  child memories, so use Node Memory only for the additional
  continuation-relevant state required by that contract.
* Treat `[U#]` anchors as internal Node Memory references. Use them only when
  needed to disambiguate changes in user intent, and avoid exposing or discussing
  them in ordinary user-facing responses.

Execution rules:

* Once context ownership and lifecycle determine the node boundaries, complete
  work in as few assistant turns as practical while minimizing total context
  pressure, roughly the sum of visible context across assistant turns. Issue
  all compatible ready tool calls in the same turn and, in code mode, within
  the same `exec` call. Use at most one Spine transition (`open`, `next`, or
  `close`) per turn. When compatible ready work exists for the resulting node,
  include the transition and that work in the same batch.
* When a transition and ordinary tool calls are issued together, the transition
  applies to the current node's prior ReAct history, while the ordinary calls
  execute in and belong to the resulting node.
* `<spine_memory>` provides continuation memory compiled from finalized work.
* Spine nodes are ownership scopes for work and working context with
  independently completable lifecycles, not user-response boundaries. Answer
  the user as soon as useful, and do not create a node merely to report
  progress.

</spine_view>
"#;

const SPINE_VIEW_START_MARKER: &str = "\n\n<spine_view>";
const MAX_SPINE_INSTRUCTION_BYTES: usize = 32 * 1024;
// The Trim segment is intentionally empty until its model-visible copy is approved.
const SPINE_TRIM_INSTRUCTIONS: &str = "";

pub(crate) fn validate_override(instructions: &str) -> Result<(), String> {
    let instructions = instructions.trim();
    if instructions.len() > MAX_SPINE_INSTRUCTION_BYTES {
        return Err(format!(
            "contents exceed the {MAX_SPINE_INSTRUCTION_BYTES}-byte limit"
        ));
    }
    let Some(body) = instructions
        .strip_prefix("<spine_view>")
        .and_then(|contents| contents.strip_suffix("</spine_view>"))
    else {
        return Err("contents must be one complete <spine_view> block".to_string());
    };
    if body.contains("<spine_view>") || body.contains("</spine_view>") {
        return Err("contents must contain exactly one <spine_view> block".to_string());
    }
    Ok(())
}

pub(crate) fn append(
    mut base_instructions: String,
    spine_jit_enabled: bool,
    spine_trim_enabled: bool,
    spine_instructions: Option<&str>,
) -> String {
    let trim_segment = spine_trim_enabled.then_some(SPINE_TRIM_INSTRUCTIONS);
    if !spine_jit_enabled && trim_segment.map_or(true, str::is_empty) {
        return base_instructions;
    }

    let jit_segment = if spine_jit_enabled {
        if let Some(start) = base_instructions.rfind(SPINE_VIEW_START_MARKER) {
            base_instructions.truncate(start);
        }
        Some(spine_instructions.unwrap_or(SPINE_JIT_INSTRUCTIONS))
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
        assert_eq!(append(base.clone(), false, false, None), base);
    }

    #[test]
    fn enabled_instructions_are_idempotent() {
        let once = append("base".to_string(), true, false, None);
        assert_eq!(append(once.clone(), true, false, None), once);
    }

    #[test]
    fn enabled_instructions_replace_an_existing_spine_segment() {
        let replaced = append(
            "base\n\n<spine_view>old instructions</spine_view>".to_string(),
            true,
            false,
            None,
        );
        assert!(!replaced.contains("old instructions"));
        assert_eq!(replaced.matches("<spine_view>").count(), 1);
    }

    #[test]
    fn trim_instructions_are_independent_and_idempotent() {
        let once = append("base".to_string(), false, true, None);
        assert_eq!(once, "base");
        assert_eq!(append(once.clone(), false, true, None), once);
    }

    #[test]
    fn configured_override_replaces_the_embedded_segment() {
        let instructions = "<spine_view>\nSPINE_OVERRIDE_SENTINEL\n</spine_view>";
        let actual = append("base".to_string(), true, false, Some(instructions));
        assert_eq!(actual, format!("base\n\n{instructions}"));
    }

    #[test]
    fn configured_override_requires_one_complete_bounded_block() {
        assert!(validate_override(SPINE_JIT_INSTRUCTIONS).is_ok());
        assert!(validate_override("missing wrapper").is_err());
        assert!(
            validate_override("<spine_view>one</spine_view><spine_view>two</spine_view>").is_err()
        );

        let oversized = format!(
            "<spine_view>{}</spine_view>",
            "x".repeat(MAX_SPINE_INSTRUCTION_BYTES)
        );
        assert!(validate_override(&oversized).is_err());
    }
}
