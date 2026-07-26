use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) const SPINE_NAMESPACE: &str = "spine";
pub(crate) const SPINE_OPEN: &str = "open";
pub(crate) const SPINE_CLOSE: &str = "close";
pub(crate) const SPINE_NEXT: &str = "next";
pub(crate) const SPINE_SPAWN: &str = "spawn";
pub(crate) const SPINE_TRIM: &str = "trim";

const NODE_MEMORY_DESCRIPTION: &str = concat!(
    "Continuation state replacing the finalized node's local working detail. ",
    "Preserve only what later work needs beyond inherited context: completed or confirmed progress, confirmed findings, decisions and constraints, validation results, bounded unresolved factual gaps or risks, remaining work that can proceed from this memory and inherited context without reconstructing the replaced detail, and the logic linking evidence and findings to decisions and next steps. ",
    "Include compact supporting evidence or precise, recoverable references when needed. ",
    "For source code, cite exact paths and lines; for commands, cite the exact command and decisive output or result, so continuation need not replay the work. ",
    "Runtime preserves user messages and child memories. ",
    "Use existing `[U#]` anchors only to bind approvals, corrections, rejections, clarifications, and elliptical replies to their referents and record the resulting continuation-relevant semantic deltas in task scope, decisions, constraints, progress, and remaining obligations; the underlying user messages remain available independently of these references."
);

const OPEN_SUMMARY_DESCRIPTION: &str = "Concise, actionable, completable goal for a direct child within one aligned set of task, information, and context-lifecycle boundaries. The call carrying it remains in the child's context.";
const NEXT_SUMMARY_DESCRIPTION: &str = "Concise, actionable, completable goal for a true sibling within its own aligned set of task, information, and context-lifecycle boundaries. The call carrying it remains in the sibling's context; finalized-node state belongs in memory.";
const TRIM_DESCRIPTION: &str = "Conservatively trim one tagged tool-result projection without changing the Spine tree or creating memory. A TRIM_ID is valid only for the immediately preceding tool-result batch and expires after the next assistant tool request; after a miss, do not retry it. Use slice to retain needed evidence, use snip only after useful facts are preserved, and otherwise leave the result unchanged.";
const SPAWN_DESCRIPTION: &str = concat!(
    "Run two or more self-contained tasks concurrently in independent child sessions created from the current full history. ",
    "Each child must own a semantically independent direction: either resolve a concrete uncertainty or produce an independently verifiable outcome, with an explicit scope, evidence boundary, and completion predicate. ",
    "It must evolve its hypotheses and approach using its own primary evidence, without later parent or sibling input, and return one terminal final memory. ",
    "For exploration or review, treat inherited analytical conclusions as hypotheses to verify, refine, or falsify against primary evidence. ",
    "The parent waits for all children and imports their terminal results as closed children atomically in input order. ",
    "Use spawn proactively when the current node contains two or more independent, self-contained workstreams and parallel execution would materially improve speed or result quality. ",
    "Do not spawn paraphrased workstreams over the same tightly coupled question unless they are deliberately assigned as independent replication or falsification. ",
    "Child workspace and external effects are non-transactional, so only dispatch tasks whose writes are non-conflicting or explicitly coordinated."
);

pub(crate) fn create_spine_tool(name: &str) -> ToolSpec {
    let function = match name {
        SPINE_OPEN => ResponsesApiTool {
            name: SPINE_OPEN.to_string(),
            description: "Enter a direct child under the current Spine cursor for one scoped task whose focused unknowns are expected to produce independently compactable local detail, beginning its local context lifecycle. Co-issued ordinary tools belong to the child; the transition applies to the current node's prior ReAct history.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "summary".to_string(),
                    JsonSchema::string(Some(OPEN_SUMMARY_DESCRIPTION.to_string())),
                )]),
                Some(vec!["summary".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        },
        SPINE_CLOSE => ResponsesApiTool {
            name: SPINE_CLOSE.to_string(),
            description: "Finalize the current node after its local result is complete or precisely bounded for continuation, replace its local detail with the supplied continuation memory, and return to its immediate parent. Root epochs cannot be closed. Co-issued ordinary tools belong to the parent; the transition applies to the current node's prior ReAct history.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "memory".to_string(),
                    JsonSchema::string(Some(NODE_MEMORY_DESCRIPTION.to_string())),
                )]),
                Some(vec!["memory".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        },
        SPINE_NEXT => ResponsesApiTool {
            name: SPINE_NEXT.to_string(),
            description: "Finalize the current node after its local result is complete or precisely bounded for continuation, replace its local detail with the supplied continuation memory, and enter a distinct sibling lifecycle under the same parent. Co-issued ordinary tools belong to the sibling; the transition applies to the current node's prior ReAct history.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    (
                        "summary".to_string(),
                        JsonSchema::string(Some(NEXT_SUMMARY_DESCRIPTION.to_string())),
                    ),
                    (
                        "memory".to_string(),
                        JsonSchema::string(Some(NODE_MEMORY_DESCRIPTION.to_string())),
                    ),
                ]),
                Some(vec!["summary".to_string(), "memory".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        },
        SPINE_SPAWN => {
            let task = JsonSchema::object(
                BTreeMap::from([
                    (
                        "summary".to_string(),
                        JsonSchema::string(Some(
                            "Concise label for one self-contained child task.".to_string(),
                        )),
                    ),
                    (
                        "prompt".to_string(),
                        JsonSchema::string(Some(
                            "Complete task instruction solvable from inherited context without parent follow-up."
                                .to_string(),
                        )),
                    ),
                ]),
                Some(vec!["summary".to_string(), "prompt".to_string()]),
                Some(false.into()),
            );
            ResponsesApiTool {
                name: SPINE_SPAWN.to_string(),
                description: SPAWN_DESCRIPTION.to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::from([(
                        "tasks".to_string(),
                        JsonSchema::array(
                            task,
                            Some("Ordered self-contained child tasks.".to_string()),
                        )
                        .with_min_items(2),
                    )]),
                    Some(vec!["tasks".to_string()]),
                    Some(false.into()),
                ),
                output_schema: None,
            }
        }
        _ => panic!("unknown Spine tool: {name}"),
    };

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SPINE_NAMESPACE.to_string(),
        description: "Use Spine to shape the work.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(function)],
    })
}

pub(crate) fn create_spine_trim_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "TRIM_ID".to_string(),
            JsonSchema::string(Some(
                "Trim id attached to a tool response in the immediately previous tool-result batch; it expires after your next assistant tool request."
                    .to_string(),
            )),
        ),
        (
            "op".to_string(),
            JsonSchema::string_enum(
                vec![serde_json::json!("snip"), serde_json::json!("slice")],
                Some("Use snip only when useful facts are preserved elsewhere; use slice to keep the needed head, tail, or anchor window.".to_string()),
            ),
        ),
        (
            "head".to_string(),
            JsonSchema::integer(Some("For op=\"slice\", keep this many characters from the start of the current visible body. Mutually exclusive with tail and anchor.".to_string())),
        ),
        (
            "tail".to_string(),
            JsonSchema::integer(Some("For op=\"slice\", keep this many characters from the end of the current visible body. Mutually exclusive with head and anchor.".to_string())),
        ),
        (
            "anchor".to_string(),
            JsonSchema::string(Some("For op=\"slice\", locate this non-empty text in the current visible body and keep an anchor window. Mutually exclusive with head and tail.".to_string())),
        ),
        (
            "preceding".to_string(),
            JsonSchema::integer(Some("For anchor slice, keep this many complete lines before the anchor line.".to_string())),
        ),
        (
            "following".to_string(),
            JsonSchema::integer(Some("For anchor slice, keep this many complete lines after the anchor line.".to_string())),
        ),
    ]);
    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SPINE_NAMESPACE.to_string(),
        description: "Use Spine to shape the work.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: SPINE_TRIM.to_string(),
            description: TRIM_DESCRIPTION.to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["TRIM_ID".to_string(), "op".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_schema_exposes_only_control_tools() {
        for name in [SPINE_OPEN, SPINE_CLOSE, SPINE_NEXT, SPINE_SPAWN] {
            let ToolSpec::Namespace(namespace) = create_spine_tool(name) else {
                panic!("expected namespace spec");
            };
            assert_eq!(namespace.tools.len(), 1);
            let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
            assert_eq!(function.name, name);
            assert!(!function.name.contains("tree"));
        }
    }

    #[test]
    fn close_and_next_require_memory() {
        for (name, required) in [
            (SPINE_CLOSE, vec!["memory"]),
            (SPINE_NEXT, vec!["summary", "memory"]),
        ] {
            let ToolSpec::Namespace(namespace) = create_spine_tool(name) else {
                panic!("expected namespace spec");
            };
            let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
            let schema = serde_json::to_value(&function.parameters).unwrap();
            assert_eq!(schema["required"], serde_json::json!(required));
            assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        }
    }

    #[test]
    fn trim_schema_requires_id_and_operation() {
        let ToolSpec::Namespace(namespace) = create_spine_trim_tool() else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
        assert_eq!(function.name, SPINE_TRIM);
        let schema = serde_json::to_value(&function.parameters).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["TRIM_ID", "op"]));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn spawn_schema_requires_two_exact_task_objects() {
        let ToolSpec::Namespace(namespace) = create_spine_tool(SPINE_SPAWN) else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
        let schema = serde_json::to_value(&function.parameters).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["tasks"]));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(schema["properties"]["tasks"]["minItems"], 2);
        assert_eq!(
            schema["properties"]["tasks"]["items"]["required"],
            serde_json::json!(["summary", "prompt"])
        );
        assert_eq!(
            schema["properties"]["tasks"]["items"]["additionalProperties"],
            serde_json::json!(false)
        );
    }
}
