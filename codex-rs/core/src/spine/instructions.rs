use spine_core::{SpineConfig, SpineRegistration};

pub(crate) fn append(
    base_instructions: String,
    config: &SpineConfig,
    registration: &SpineRegistration,
) -> String {
    config.extend_system_prompt(&base_instructions, registration)
}
