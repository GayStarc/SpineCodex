use super::*;
use color_eyre::eyre::WrapErr;
use pretty_assertions::assert_eq;
use std::path::Path;

#[test]
fn app_scoped_key_path_quotes_dotted_app_ids() {
    assert_eq!(
        app_scoped_key_path("plugin.linear", "enabled"),
        "apps.\"plugin.linear\".enabled"
    );
}

#[test]
fn trusted_project_edit_targets_project_trust_level() {
    assert_eq!(
        trusted_project_edit(Path::new("/workspace/team.project")),
        ConfigEdit {
            key_path: "projects.\"/workspace/team.project\".trust_level".to_string(),
            value: serde_json::json!("trusted"),
            merge_strategy: MergeStrategy::Replace,
        }
    );
}

#[test]
fn format_config_error_preserves_server_validation_message() {
    let err = Err::<(), _>(color_eyre::eyre::eyre!(
        "config/batchWrite failed: Invalid configuration: features.fast_mode=true violates \
         managed requirements; allowed set [fast_mode=false]"
    ))
    .wrap_err("config/batchWrite failed in TUI")
    .unwrap_err();

    assert_eq!(
        format_config_error(&err),
        "config/batchWrite failed in TUI: config/batchWrite failed: Invalid configuration: \
         features.fast_mode=true violates managed requirements; allowed set [fast_mode=false]"
    );
}

#[test]
fn enabling_default_disabled_spine_spawn_persists_explicit_true() {
    assert_eq!(
        build_feature_enabled_edit("spine_spawn", /*enabled*/ true),
        ConfigEdit {
            key_path: "features.spine_spawn".to_string(),
            value: serde_json::json!(true),
            merge_strategy: MergeStrategy::Replace,
        }
    );
}

#[test]
fn disabling_default_disabled_spine_spawn_clears_the_override() {
    assert_eq!(
        build_feature_enabled_edit("spine_spawn", /*enabled*/ false),
        ConfigEdit {
            key_path: "features.spine_spawn".to_string(),
            value: serde_json::Value::Null,
            merge_strategy: MergeStrategy::Replace,
        }
    );
}

#[test]
fn spine_spawn_settings_use_a_boolean_feature_and_rebuild_the_top_level_table() {
    assert_eq!(
        build_spine_spawn_settings_edits(/*enabled*/ true, /*max_threads*/ 10),
        vec![
            ConfigEdit {
                key_path: "features.spine_spawn".to_string(),
                value: serde_json::json!(true),
                merge_strategy: MergeStrategy::Replace,
            },
            ConfigEdit {
                key_path: "spine_spawn".to_string(),
                value: serde_json::Value::Null,
                merge_strategy: MergeStrategy::Replace,
            },
            ConfigEdit {
                key_path: "spine_spawn".to_string(),
                value: serde_json::json!({
                    "max_concurrent_threads_per_session": 10,
                }),
                merge_strategy: MergeStrategy::Replace,
            },
        ]
    );
}
