pub(crate) const SPINE_JIT_INSTRUCTIONS: &str = r#"<spine_instruction>
All work must be organized recursively in the SpineTree. Every piece of work
belongs to the active branch. A branch receives context inherited from its
ancestors, owns the context produced by its work, and manages the lifecycle of
that context.

Open a child whenever a bounded subcomputation is likely to produce a
self-contained result for its surrounding work. Prefer opening at a potential
boundary before the local detail accumulates; you do not need to prove the
boundary before opening. Apply this rule recursively within every active
branch. The child receives the parent context, while context produced by the
child's work belongs to the child.

When the local objective is complete, prefer finalizing the current branch
if replacing its exact local context with returned memory is expected to
benefit the remaining work. Weigh the context saved against the possibility
that omitted detail may later need to be reloaded or reconstructed. Use
`spine.close` when control should return to the parent, or `spine.next` when
another sibling scope should begin under the same parent. Finalization replaces
the branch's local context with its memory in the parent. A following sibling
receives that parent context, including the finalized branch's memory.

Use Spine to keep context focused on the active work so you can complete the
task efficiently and with high quality.

Notes:

1. `<spine_memory>` contains memory from finalized branches.
2. Answer the user regardless of which branch you are in.
3. Each ReAct interaction may issue at most one of `spine.open`, `spine.close`,
   or `spine.next`. It may issue that transition with ordinary tool calls in
   the same `exec`. The transition applies to the active branch's prior ReAct
   history, while the ordinary tool calls execute in the resulting branch.

</spine_instruction>
"#;

const SPINE_INSTRUCTION_START_MARKER: &str = "\n\n<spine_instruction>";
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
        .strip_prefix("<spine_instruction>")
        .and_then(|contents| contents.strip_suffix("</spine_instruction>"))
    else {
        return Err("contents must be one complete <spine_instruction> block".to_string());
    };
    if body.contains("<spine_instruction>") || body.contains("</spine_instruction>") {
        return Err("contents must contain exactly one <spine_instruction> block".to_string());
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
        if let Some(start) = base_instructions.rfind(SPINE_INSTRUCTION_START_MARKER) {
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
            "base\n\n<spine_instruction>old instructions</spine_instruction>".to_string(),
            true,
            false,
            None,
        );
        assert!(!replaced.contains("old instructions"));
        assert_eq!(replaced.matches("<spine_instruction>").count(), 1);
    }

    #[test]
    fn trim_instructions_are_independent_and_idempotent() {
        let once = append("base".to_string(), false, true, None);
        assert_eq!(once, "base");
        assert_eq!(append(once.clone(), false, true, None), once);
    }

    #[test]
    fn configured_override_replaces_the_embedded_segment() {
        let instructions = "<spine_instruction>\nSPINE_OVERRIDE_SENTINEL\n</spine_instruction>";
        let actual = append("base".to_string(), true, false, Some(instructions));
        assert_eq!(actual, format!("base\n\n{instructions}"));
    }

    #[test]
    fn configured_override_requires_one_complete_bounded_block() {
        assert!(validate_override(SPINE_JIT_INSTRUCTIONS).is_ok());
        assert!(validate_override("missing wrapper").is_err());
        assert!(
            validate_override(
                "<spine_instruction>one</spine_instruction><spine_instruction>two</spine_instruction>"
            )
            .is_err()
        );

        let oversized = format!(
            "<spine_instruction>{}</spine_instruction>",
            "x".repeat(MAX_SPINE_INSTRUCTION_BYTES)
        );
        assert!(validate_override(&oversized).is_err());
    }
}
