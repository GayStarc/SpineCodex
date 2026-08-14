pub(crate) const SPINE_JIT_INSTRUCTIONS: &str = r#"<spine_instruction>
All work must be organized recursively in the SpineTree. Every piece of work
belongs to an active branch, and each branch owns the context and lifecycle of
that work. A branch receives context inherited from its ancestors and owns the
context produced by its active work. When work enters a new ownership or
lifecycle scope, issue `spine.open` in the first interaction for that work.
Apply these ownership and lifecycle rules recursively at every nested context
level and within every active branch. Keep exact context in the branch while
its work is active. When that work no longer needs its exact local context,
issue `spine.close` to continue in the parent or `spine.next` to continue in a
sibling, returning the needed state as memory.
Use Spine to manage context and work ownership so you can stay focused on the
active work and complete the task efficiently and with high quality.

Notes:

1. `<spine_memory>` provides memory returned by finalized branches.
2. Answer the user regardless of which branch you are in.
3. Spine `spine.open`, `spine.close`, and `spine.next` can be issued with
   ordinary tool calls in the same `exec`; the transition applies to the active
   branch's prior ReAct history, and the ordinary calls execute in the
   resulting branch.

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
