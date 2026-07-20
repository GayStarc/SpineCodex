use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_tools::parse_tool_input_schema_without_compaction;
use spine_core::ToolDefinition;

pub(crate) const SPINE_NAMESPACE: &str = spine_core::SPINE_NAMESPACE;
pub(crate) const SPINE_OPEN: &str = "open";
pub(crate) const SPINE_CLOSE: &str = "close";
pub(crate) const SPINE_NEXT: &str = "next";
pub(crate) const SPINE_SPAWN: &str = "spawn";
pub(crate) const SPINE_TRIM: &str = "trim";

pub(crate) fn create_spine_tool(definition: &ToolDefinition) -> ToolSpec {
    let parameters: JsonSchema = parse_tool_input_schema_without_compaction(&definition.parameters)
        .expect("Spine SDK emits valid JSON schemas");
    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SPINE_NAMESPACE.to_string(),
        description: "Use Spine to shape the work.".to_string(),
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
    use spine_core::SpineRegistration;
    use spine_core::SpineTool;
    use spine_core::ToolCatalog;

    #[test]
    fn sdk_specs_match_the_pre_sdk_model_surface() {
        let registration = SpineRegistration::builder()
            .enable(Feature::Jit)
            .enable(Feature::Trim)
            .enable(Feature::Spawn)
            .build()
            .unwrap();
        let catalog = ToolCatalog::new(&SpineConfig::v1(), &registration).unwrap();

        for tool in [
            SpineTool::Open,
            SpineTool::Close,
            SpineTool::Next,
            SpineTool::Spawn,
        ] {
            let actual = create_spine_tool(catalog.definition(tool).unwrap());
            let expected = crate::tools::handlers::spine_spec::create_spine_tool(tool.name());
            assert_eq!(actual, expected, "{} schema changed", tool.qualified_name());
        }

        let actual = create_spine_tool(catalog.definition(SpineTool::Trim).unwrap());
        let expected = crate::tools::handlers::spine_spec::create_spine_trim_tool();
        assert_eq!(actual, expected, "spine.trim schema changed");
    }
}
