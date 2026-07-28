use toml::Value as TomlValue;

pub(crate) fn default_empty_table() -> TomlValue {
    TomlValue::Table(Default::default())
}

pub fn build_cli_overrides_layer(cli_overrides: &[(String, TomlValue)]) -> TomlValue {
    let mut root = default_empty_table();
    for (path, value) in cli_overrides {
        let path = normalize_structured_feature_toggle(path, value);
        apply_toml_override(&mut root, &path, value.clone());
    }
    root
}

fn normalize_structured_feature_toggle(path: &str, value: &TomlValue) -> String {
    if path == "features.spine_spawn" && value.is_bool() {
        format!("{path}.enabled")
    } else {
        path.to_string()
    }
}

/// Apply a single dotted-path override onto a TOML value.
fn apply_toml_override(root: &mut TomlValue, path: &str, value: TomlValue) {
    use toml::value::Table;

    let mut current = root;
    let mut segments_iter = path.split('.').peekable();

    while let Some(segment) = segments_iter.next() {
        let is_last = segments_iter.peek().is_none();

        if is_last {
            match current {
                TomlValue::Table(table) => {
                    table.insert(segment.to_string(), value);
                }
                _ => {
                    let mut table = Table::new();
                    table.insert(segment.to_string(), value);
                    *current = TomlValue::Table(table);
                }
            }
            return;
        }

        match current {
            TomlValue::Table(table) => {
                current = table
                    .entry(segment.to_string())
                    .or_insert_with(|| TomlValue::Table(Table::new()));
            }
            _ => {
                *current = TomlValue::Table(Table::new());
                if let TomlValue::Table(tbl) = current {
                    current = tbl
                        .entry(segment.to_string())
                        .or_insert_with(|| TomlValue::Table(Table::new()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build_cli_overrides_layer;
    use crate::merge_toml_values;
    use toml::Value as TomlValue;

    #[test]
    fn spine_spawn_toggle_preserves_structured_capacity() {
        let mut base: TomlValue = toml::from_str(
            r#"[features.spine_spawn]
max_concurrent_threads_per_session = 3
"#,
        )
        .expect("valid base config");
        let overlay = build_cli_overrides_layer(&[(
            "features.spine_spawn".to_string(),
            TomlValue::Boolean(true),
        )]);

        merge_toml_values(&mut base, &overlay);

        assert_eq!(
            base["features"]["spine_spawn"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            base["features"]["spine_spawn"]["max_concurrent_threads_per_session"].as_integer(),
            Some(3)
        );
    }
}
