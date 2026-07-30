use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_tools::parse_tool_input_schema_without_compaction;
use spine_core::ToolDefinition;

pub(crate) const SPINE_NAMESPACE: &str = spine_core::SPINE_NAMESPACE;
pub(crate) const SPINE_OPEN: &str = spine_core::SpineTool::Open.name();
pub(crate) const SPINE_CLOSE: &str = spine_core::SpineTool::Close.name();
pub(crate) const SPINE_NEXT: &str = spine_core::SpineTool::Next.name();
pub(crate) const SPINE_SPAWN: &str = spine_core::SpineTool::Spawn.name();
pub(crate) const SPINE_TRIM: &str = spine_core::SpineTool::Trim.name();

pub(crate) fn create_spine_tool(definition: &ToolDefinition) -> ToolSpec {
    let parameters: JsonSchema = parse_tool_input_schema_without_compaction(&definition.parameters)
        .expect("Spine SDK emits valid JSON schemas");
    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SPINE_NAMESPACE.to_string(),
        description: spine_core::SPINE_NAMESPACE_DESCRIPTION.to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: definition.tool.name().to_string(),
            description: definition.description.clone(),
            strict: false,
            defer_loading: None,
            parameters,
            output_schema: None,
        })],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spine_core::Feature;
    use spine_core::SpineConfig;
    use spine_core::SpineTool;
    use spine_core::ToolCatalog;

    #[test]
    fn sdk_specs_match_the_pre_sdk_model_surface() {
        let config = SpineConfig::v1()
            .with_features([Feature::Jit, Feature::Trim, Feature::Spawn])
            .unwrap();
        let catalog = ToolCatalog::new(&config).unwrap().with_spawn_max_items(3);

        for tool in [
            SpineTool::Open,
            SpineTool::Close,
            SpineTool::Next,
            SpineTool::Spawn,
        ] {
            let actual = create_spine_tool(catalog.definition(tool).unwrap());
            let expected = match tool {
                SpineTool::Open | SpineTool::Close | SpineTool::Next => {
                    crate::tools::handlers::spine_spec::create_spine_tool(tool.name())
                }
                SpineTool::Spawn => crate::tools::handlers::spine_spec::create_spine_spawn_tool(3),
                SpineTool::Trim => unreachable!("trim is compared through its dedicated factory"),
            };
            assert_eq!(actual, expected, "{} schema changed", tool.qualified_name());
        }

        let actual = create_spine_tool(catalog.definition(SpineTool::Trim).unwrap());
        let expected = crate::tools::handlers::spine_spec::create_spine_trim_tool();
        assert_eq!(actual, expected, "spine.trim schema changed");
    }
}
