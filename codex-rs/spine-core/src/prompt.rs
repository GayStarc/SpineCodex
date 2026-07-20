//! Configured, feature-gated prompt extension.

use crate::{Feature, SpineConfig, SpineRegistration};

const SPINE_VIEW_START_MARKER: &str = "\n\n<spine_view>";

pub(crate) fn extend(
    mut base: String,
    config: &SpineConfig,
    registration: &SpineRegistration,
) -> String {
    if registration.is_enabled(Feature::Jit)
        && let Some(start) = base.rfind(SPINE_VIEW_START_MARKER)
    {
        base.truncate(start);
    }

    let segments = [Feature::Jit, Feature::Trim, Feature::Spawn]
        .into_iter()
        .filter(|feature| registration.is_enabled(*feature))
        .map(|feature| config.prompt(feature))
        .filter(|segment| !segment.is_empty());
    for segment in segments {
        if base.contains(segment) {
            continue;
        }
        if !base.is_empty() {
            base.push_str("\n\n");
        }
        base.push_str(segment);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_off_is_identity() {
        let config = SpineConfig::v1();
        let registration = SpineRegistration::builder().build().unwrap();
        assert_eq!(extend("base".to_string(), &config, &registration), "base");
    }

    #[test]
    fn configured_segment_is_idempotent() {
        let config = SpineConfig::parse_toml(
            r#"schema_version = 1
[limits]
trim_threshold_bytes = 100
[prompt]
jit = "<spine_view>jit</spine_view>"
[tools.open]
description = "open"
[tools.close]
description = "close"
[tools.next]
description = "next"
"#,
        )
        .unwrap();
        let registration = SpineRegistration::builder()
            .enable(Feature::Jit)
            .build()
            .unwrap();
        let once = extend("base".to_string(), &config, &registration);
        assert_eq!(extend(once.clone(), &config, &registration), once);
    }
}
