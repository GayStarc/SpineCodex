use super::*;
use pretty_assertions::assert_eq;

#[test]
fn typed_fragments_own_exact_rendering() {
    let node = SpineNodeFragment::new(
        &NodeId::root_epoch(1).child(1),
        "child <scope>",
        NodeStatus::Live,
    )
    .unwrap();
    let memory = SpineMemoryFragment::new(&NodeId::root_epoch(1), "finished").unwrap();

    assert_eq!(
        node.render(),
        r#"<spine_node id="1.1" summary="child &lt;scope&gt;" status="live" />"#
    );
    assert_eq!(
        memory.render(),
        "<spine_memory node_id=\"1\">\nfinished\n</spine_memory>"
    );
}

#[test]
fn final_rendered_fragment_has_a_hard_byte_limit() {
    let result = SpineMemoryFragment::new(
        &NodeId::root_epoch(1),
        &"x".repeat(MAX_SPINE_FRAGMENT_BYTES),
    );

    assert!(result.is_err());
}
