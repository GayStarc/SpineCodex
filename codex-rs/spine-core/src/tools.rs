use crate::Feature;
use crate::SpineRegistration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCatalog {
    names: Vec<&'static str>,
}

impl ToolCatalog {
    pub(crate) fn from_registration(registration: &SpineRegistration) -> Self {
        let mut names = Vec::new();
        if registration.is_enabled(Feature::Jit) {
            names.extend(["spine.open", "spine.close", "spine.next"]);
        }
        if registration.is_enabled(Feature::Trim) {
            names.push("spine.trim");
        }
        if registration.is_enabled(Feature::Spawn) {
            names.push("spine.spawn");
        }
        Self { names }
    }

    pub fn names(&self) -> &[&'static str] {
        &self.names
    }
}
