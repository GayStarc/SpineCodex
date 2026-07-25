use super::spine_spawn_progress::SpineSpawnOverlay;
use super::*;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SpineSpawnOutcome;
use codex_app_server_protocol::SpineSpawnProgressUpdatedNotification;
use codex_app_server_protocol::SpineTreeNode;
use codex_app_server_protocol::SpineTreeNodeKind;
use codex_app_server_protocol::SpineTreeNodeStatus;
use codex_app_server_protocol::SpineTreeUpdatedNotification;
use std::collections::HashSet;

use crate::motion::MotionMode;
use crate::motion::green_breathing_marker;
use crate::motion::white_green_shimmer_text;

#[path = "spine_tree_debug.rs"]
mod debug;

const PRETTY_MAX_VISIBLE_SIBLINGS: usize = 3;
const INVALID_SPINE_TREE_SNAPSHOT_LABEL: &str = "invalid Spine tree snapshot";

#[cfg(test)]
pub(crate) fn new_spine_tree_snapshot(
    snapshot: SpineTreeUpdatedNotification,
) -> SpineTreeUpdateCell {
    SpineTreeUpdateCell {
        snapshot,
        display_mode: SpineTreeDisplayMode::Pretty,
        spawn_overlays: Vec::new(),
        animations_enabled: false,
        active_working: false,
    }
}

pub(crate) fn new_debug_spine_tree_snapshot(
    snapshot: SpineTreeUpdatedNotification,
) -> SpineTreeUpdateCell {
    SpineTreeUpdateCell {
        snapshot,
        display_mode: SpineTreeDisplayMode::Debug(None),
        spawn_overlays: Vec::new(),
        animations_enabled: false,
        active_working: false,
    }
}

pub(crate) fn new_debug_spine_node_snapshot(
    snapshot: SpineTreeUpdatedNotification,
    node_id: String,
) -> SpineTreeUpdateCell {
    SpineTreeUpdateCell {
        snapshot,
        display_mode: SpineTreeDisplayMode::Debug(Some(node_id)),
        spawn_overlays: Vec::new(),
        animations_enabled: false,
        active_working: false,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SpineTreeViewState {
    snapshot: Option<SpineTreeUpdatedNotification>,
    overlays: Vec<SpineSpawnOverlay>,
    settled_spawn_call_ids: HashSet<String>,
    animations_enabled: bool,
}

impl Default for SpineTreeViewState {
    fn default() -> Self {
        Self::new(false)
    }
}

impl SpineTreeViewState {
    pub(crate) fn new(animations_enabled: bool) -> Self {
        Self {
            snapshot: None,
            overlays: Vec::new(),
            settled_spawn_call_ids: HashSet::new(),
            animations_enabled,
        }
    }

    pub(crate) fn snapshot(&self) -> Option<&SpineTreeUpdatedNotification> {
        self.snapshot.as_ref()
    }

    pub(crate) fn apply_tree_update(&mut self, snapshot: SpineTreeUpdatedNotification) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| snapshot.snapshot_seq < current.snapshot_seq)
        {
            return;
        }
        let removed_call_ids = snapshot.settled_spawn_call_ids.clone();
        self.snapshot = Some(snapshot);
        self.settled_spawn_call_ids
            .extend(removed_call_ids.iter().cloned());
        self.overlays.retain(|overlay| {
            !removed_call_ids
                .iter()
                .any(|call_id| call_id == overlay.call_id())
        });
    }

    pub(crate) fn clear_incomplete_spawn_overlays(&mut self, turn_id: Option<&str>) -> bool {
        let before = self.overlays.len();
        self.overlays
            .retain(|overlay| turn_id.is_some_and(|turn_id| overlay.turn_id() != turn_id));
        self.overlays.len() != before
    }

    pub(crate) fn apply_spawn_progress(
        &mut self,
        notification: SpineSpawnProgressUpdatedNotification,
    ) {
        if self
            .settled_spawn_call_ids
            .contains(notification.call_id.as_str())
        {
            return;
        }
        if let Some(overlay) = self.overlays.iter_mut().find(|overlay| {
            overlay.turn_id() == notification.turn_id && overlay.call_id() == notification.call_id
        }) {
            overlay.replace_notification(notification);
        } else {
            self.overlays.push(SpineSpawnOverlay::new(notification));
        }
    }

    pub(crate) fn seed_activity(
        &mut self,
        turn_id: &str,
        call_id: &str,
        thread_id: &str,
        notifications: impl Iterator<Item = ServerNotification>,
    ) -> bool {
        self.overlays
            .iter_mut()
            .find(|overlay| overlay.turn_id() == turn_id && overlay.call_id() == call_id)
            .is_some_and(|overlay| overlay.seed_activity(thread_id, notifications))
    }

    pub(crate) fn overlay_key_for_child_thread(&self, thread_id: &str) -> Option<(String, String)> {
        self.overlays
            .iter()
            .find(|overlay| overlay.has_child_thread(thread_id))
            .map(|overlay| (overlay.turn_id().to_string(), overlay.call_id().to_string()))
    }

    pub(crate) fn is_activity_seeded(&self, turn_id: &str, call_id: &str, thread_id: &str) -> bool {
        self.overlays
            .iter()
            .find(|overlay| overlay.turn_id() == turn_id && overlay.call_id() == call_id)
            .is_some_and(|overlay| overlay.has_activity(thread_id))
    }

    pub(crate) fn apply_activity(
        &mut self,
        turn_id: &str,
        call_id: &str,
        thread_id: &str,
        notification: &ServerNotification,
        status: Option<codex_app_server_protocol::CollabAgentStatus>,
    ) -> bool {
        self.overlays
            .iter_mut()
            .filter(|overlay| {
                overlay.turn_id() == turn_id
                    && overlay.call_id() == call_id
                    && overlay.has_child_thread(thread_id)
            })
            .map(|overlay| overlay.update_activity(thread_id, notification, status.clone()))
            .any(|changed| changed)
    }

    pub(crate) fn update_status(
        &mut self,
        turn_id: &str,
        call_id: &str,
        thread_id: &str,
        status: codex_app_server_protocol::CollabAgentStatus,
    ) -> bool {
        self.overlays
            .iter_mut()
            .filter(|overlay| {
                overlay.turn_id() == turn_id
                    && overlay.call_id() == call_id
                    && overlay.has_child_thread(thread_id)
            })
            .map(|overlay| overlay.update_status(thread_id, status.clone()))
            .any(|changed| changed)
    }

    pub(crate) fn render_cell(&self) -> Option<SpineTreeUpdateCell> {
        if self.overlays.is_empty() {
            return None;
        }
        let snapshot = self.snapshot.clone()?;
        Some(SpineTreeUpdateCell {
            snapshot,
            display_mode: SpineTreeDisplayMode::Pretty,
            spawn_overlays: self.overlays.clone(),
            animations_enabled: self.animations_enabled,
            active_working: true,
        })
    }

    pub(crate) fn snapshot_cell(&self) -> Option<SpineTreeUpdateCell> {
        let snapshot = self.snapshot.clone()?;
        Some(SpineTreeUpdateCell {
            snapshot,
            display_mode: SpineTreeDisplayMode::Pretty,
            spawn_overlays: Vec::new(),
            animations_enabled: false,
            active_working: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn has_spawn_call(&self, call_id: &str) -> bool {
        self.overlays
            .iter()
            .any(|overlay| overlay.call_id() == call_id)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SpineTreeUpdateCell {
    snapshot: SpineTreeUpdatedNotification,
    display_mode: SpineTreeDisplayMode,
    spawn_overlays: Vec<SpineSpawnOverlay>,
    animations_enabled: bool,
    active_working: bool,
}

#[derive(Debug, Clone)]
enum SpineTreeDisplayMode {
    Pretty,
    Debug(Option<String>),
}

impl SpineTreeUpdateCell {
    fn active_working_started_at(&self) -> Option<std::time::Instant> {
        if !self.active_working {
            return None;
        }
        self.spawn_overlays
            .iter()
            .map(SpineSpawnOverlay::animation_start)
            .min()
    }
}

impl HistoryCell for SpineTreeUpdateCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        match &self.display_mode {
            SpineTreeDisplayMode::Pretty => pretty_display_lines(
                &self.snapshot,
                &self.spawn_overlays,
                width,
                self.animations_enabled,
                self.active_working_started_at(),
            ),
            SpineTreeDisplayMode::Debug(node_id) => {
                debug::display_lines(&self.snapshot, width, node_id.as_deref())
            }
        }
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        match &self.display_mode {
            SpineTreeDisplayMode::Pretty => pretty_raw_lines(&self.snapshot),
            SpineTreeDisplayMode::Debug(node_id) => {
                debug::raw_lines(&self.snapshot, node_id.as_deref())
            }
        }
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        if !self.animations_enabled {
            return None;
        }
        let started_at = self.active_working_started_at()?;
        Some(started_at.elapsed().as_millis() as u64 / 50)
    }
}

fn pretty_display_lines(
    snapshot: &SpineTreeUpdatedNotification,
    overlays: &[SpineSpawnOverlay],
    width: u16,
    animations_enabled: bool,
    active_working_started_at: Option<std::time::Instant>,
) -> Vec<Line<'static>> {
    let mut lines = vec![pretty_header(snapshot)];
    if let Err(error) = validate_spine_tree_snapshot(snapshot) {
        lines.push(invalid_snapshot_display_line(error));
        return lines;
    }

    let root_nodes = visible_pretty_nodes(snapshot, &child_nodes(snapshot, None));
    let overlays_at_root = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == snapshot.active_node_id)
        .is_some_and(|node| {
            should_elide_pretty_node(
                node,
                !child_nodes(snapshot, Some(node.node_id.as_str())).is_empty(),
                true,
            )
        });
    if root_nodes.is_empty() && !(overlays_at_root && !overlays.is_empty()) {
        lines.push(
            vec![
                format!("  {}", pretty_branch(true)).dim(),
                "(empty)".dim().italic(),
            ]
            .into(),
        );
        return lines;
    }

    let active_path = active_path_ids(snapshot);
    render_pretty_nodes(
        snapshot,
        overlays,
        &root_nodes,
        &active_path,
        "  ",
        width,
        &mut lines,
        overlays_at_root && !overlays.is_empty(),
        animations_enabled,
        active_working_started_at,
    );
    if overlays_at_root {
        for (index, overlay) in overlays.iter().enumerate() {
            lines.extend(overlay.display_lines(
                "  ",
                index + 1 == overlays.len(),
                width,
                animations_enabled,
            ));
        }
    }
    lines
}

fn pretty_raw_lines(snapshot: &SpineTreeUpdatedNotification) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("Spine Tree")];
    if let Err(error) = validate_spine_tree_snapshot(snapshot) {
        lines.push(invalid_snapshot_raw_line(error));
        return lines;
    }

    let root_nodes = visible_pretty_nodes(snapshot, &child_nodes(snapshot, None));
    if root_nodes.is_empty() {
        lines.push(Line::from(format!("  {}(empty)", pretty_branch(true))));
        return lines;
    }

    let active_path = active_path_ids(snapshot);
    append_pretty_raw_nodes(snapshot, &root_nodes, &active_path, "  ", &mut lines);
    lines
}

fn pretty_header(_snapshot: &SpineTreeUpdatedNotification) -> Line<'static> {
    vec!["• ".dim(), "Spine Tree".green().bold()].into()
}
fn render_pretty_node(
    snapshot: &SpineTreeUpdatedNotification,
    overlays: &[SpineSpawnOverlay],
    node: &SpineTreeNode,
    active_path: &HashSet<&str>,
    prefix: &str,
    is_last: bool,
    width: u16,
    out: &mut Vec<Line<'static>>,
    animations_enabled: bool,
    active_working_started_at: Option<std::time::Instant>,
) {
    let children = child_nodes(snapshot, Some(node.node_id.as_str()));
    let active = node.node_id == snapshot.active_node_id;
    let line_prefix = format!("{}{}", prefix, pretty_branch(is_last));
    let child_prefix = format!("{}{}", prefix, pretty_child_prefix(is_last));
    let node_is_working = active && active_working_started_at.is_some();
    let mut spans = vec![Span::from(line_prefix).dim()];
    spans.push(if node_is_working {
        green_breathing_marker(
            active_working_started_at,
            MotionMode::from_animations_enabled(animations_enabled),
            "◉",
            "◌",
        )
    } else {
        pretty_marker(node, active, !children.is_empty())
    });
    spans.push(" ".into());
    let label = pretty_node_label_text(node, active);
    if node_is_working {
        spans.extend(white_green_shimmer_text(
            &label,
            MotionMode::from_animations_enabled(animations_enabled),
        ));
    } else {
        spans.push(Span::from(label));
    }

    let line = Line::from(spans);
    let wrapped = adaptive_wrap_line(
        &line,
        RtOptions::new(width.saturating_sub(2).max(1) as usize)
            .subsequent_indent(Span::from(format!("{child_prefix}  ")).dim().into()),
    );
    push_owned_lines(&wrapped, out);

    let node_overlays = if active { overlays } else { &[] };
    render_pretty_nodes(
        snapshot,
        overlays,
        &children,
        active_path,
        &child_prefix,
        width,
        out,
        !node_overlays.is_empty(),
        animations_enabled,
        active_working_started_at,
    );
    for (index, overlay) in node_overlays.iter().enumerate() {
        out.extend(overlay.display_lines(
            &child_prefix,
            index + 1 == node_overlays.len(),
            width,
            animations_enabled,
        ));
    }
}

fn render_pretty_nodes(
    snapshot: &SpineTreeUpdatedNotification,
    overlays: &[SpineSpawnOverlay],
    nodes: &[&SpineTreeNode],
    active_path: &HashSet<&str>,
    prefix: &str,
    width: u16,
    out: &mut Vec<Line<'static>>,
    has_trailing_overlay: bool,
    animations_enabled: bool,
    active_working_started_at: Option<std::time::Instant>,
) {
    let items = pretty_render_items(snapshot, nodes, active_path);
    let item_count = items.len();
    for (index, item) in items.into_iter().enumerate() {
        let is_last = index + 1 == item_count && !has_trailing_overlay;
        match item {
            PrettySiblingItem::HistoryBucket(count) => {
                render_history_bucket(count, prefix, is_last, width, out);
            }
            PrettySiblingItem::Node(node) => {
                render_pretty_node(
                    snapshot,
                    overlays,
                    node,
                    active_path,
                    prefix,
                    is_last,
                    width,
                    out,
                    animations_enabled,
                    active_working_started_at,
                );
            }
        }
    }
}

fn append_pretty_raw_nodes(
    snapshot: &SpineTreeUpdatedNotification,
    nodes: &[&SpineTreeNode],
    active_path: &HashSet<&str>,
    prefix: &str,
    out: &mut Vec<Line<'static>>,
) {
    let items = pretty_render_items(snapshot, nodes, active_path);
    let item_count = items.len();
    for (index, item) in items.into_iter().enumerate() {
        let is_last = index + 1 == item_count;
        match item {
            PrettySiblingItem::HistoryBucket(count) => out.push(Line::from(format!(
                "{}{}◌ {}",
                prefix,
                pretty_branch(is_last),
                history_bucket_label(count)
            ))),
            PrettySiblingItem::Node(node) => {
                let children = child_nodes(snapshot, Some(node.node_id.as_str()));
                let active = node.node_id == snapshot.active_node_id;
                let marker = pretty_marker_text(node, active, !children.is_empty());
                out.push(Line::from(format!(
                    "{}{}{} {}",
                    prefix,
                    pretty_branch(is_last),
                    marker,
                    pretty_node_label_text(node, active)
                )));
                let child_prefix = format!("{}{}", prefix, pretty_child_prefix(is_last));
                append_pretty_raw_nodes(snapshot, &children, active_path, &child_prefix, out);
            }
        }
    }
}

fn pretty_render_items<'a>(
    snapshot: &'a SpineTreeUpdatedNotification,
    nodes: &[&'a SpineTreeNode],
    active_path: &HashSet<&str>,
) -> Vec<PrettySiblingItem<'a>> {
    let mut normalized_nodes = Vec::new();
    append_visible_pretty_nodes(snapshot, nodes, &mut normalized_nodes);
    pretty_sibling_items(&normalized_nodes, active_path)
}

fn visible_pretty_nodes<'a>(
    snapshot: &'a SpineTreeUpdatedNotification,
    nodes: &[&'a SpineTreeNode],
) -> Vec<&'a SpineTreeNode> {
    let mut visible = Vec::new();
    append_visible_pretty_nodes(snapshot, nodes, &mut visible);
    visible
}

fn append_visible_pretty_nodes<'a>(
    snapshot: &'a SpineTreeUpdatedNotification,
    nodes: &[&'a SpineTreeNode],
    out: &mut Vec<&'a SpineTreeNode>,
) {
    for node in nodes.iter().copied() {
        let children = child_nodes(snapshot, Some(node.node_id.as_str()));
        let active = node.node_id == snapshot.active_node_id;
        if should_elide_pretty_node(node, !children.is_empty(), active) {
            append_visible_pretty_nodes(snapshot, &children, out);
        } else {
            out.push(node);
        }
    }
}

enum PrettySiblingItem<'a> {
    HistoryBucket(usize),
    Node(&'a SpineTreeNode),
}

fn pretty_sibling_items<'a>(
    nodes: &[&'a SpineTreeNode],
    active_path: &HashSet<&str>,
) -> Vec<PrettySiblingItem<'a>> {
    let mut items = nodes
        .iter()
        .copied()
        .map(|node| {
            if bucketable_history_node(node, active_path) {
                PrettySiblingItem::HistoryBucket(1)
            } else {
                PrettySiblingItem::Node(node)
            }
        })
        .collect::<Vec<_>>();

    let active_index = nodes
        .iter()
        .position(|node| active_path.contains(node.node_id.as_str()));
    let visible_end = active_index.map_or(nodes.len(), |index| index + 1);
    if visible_end < nodes.len() {
        return merge_adjacent_history_buckets(items);
    };
    if nodes.len() <= PRETTY_MAX_VISIBLE_SIBLINGS {
        return merge_adjacent_history_buckets(items);
    }
    let visible_start = visible_end.saturating_sub(PRETTY_MAX_VISIBLE_SIBLINGS);

    let mut folded = Vec::new();
    if visible_start > 0 {
        let hidden_count = items[..visible_start]
            .iter()
            .map(pretty_sibling_item_history_count)
            .sum();
        folded.push(PrettySiblingItem::HistoryBucket(hidden_count));
    }
    folded.extend(items.drain(visible_start..visible_end));
    merge_adjacent_history_buckets(folded)
}

fn bucketable_history_node(node: &SpineTreeNode, active_path: &HashSet<&str>) -> bool {
    matches!(
        node.status,
        SpineTreeNodeStatus::Closed | SpineTreeNodeStatus::Compacted
    ) && trimmed_summary(node).is_none()
        && !active_path.contains(node.node_id.as_str())
}

fn pretty_sibling_item_history_count(item: &PrettySiblingItem<'_>) -> usize {
    match item {
        PrettySiblingItem::HistoryBucket(count) => *count,
        PrettySiblingItem::Node(_) => 1,
    }
}

fn merge_adjacent_history_buckets<'a>(
    items: Vec<PrettySiblingItem<'a>>,
) -> Vec<PrettySiblingItem<'a>> {
    let mut merged = Vec::with_capacity(items.len());
    for item in items {
        match item {
            PrettySiblingItem::HistoryBucket(count) => {
                if let Some(PrettySiblingItem::HistoryBucket(previous)) = merged.last_mut() {
                    *previous += count;
                } else {
                    merged.push(PrettySiblingItem::HistoryBucket(count));
                }
            }
            PrettySiblingItem::Node(node) => merged.push(PrettySiblingItem::Node(node)),
        }
    }
    merged
}

fn active_path_ids(snapshot: &SpineTreeUpdatedNotification) -> HashSet<&str> {
    let mut active_path = HashSet::new();
    let mut current = snapshot.active_node_id.as_str();
    active_path.insert(current);

    while let Some(node) = snapshot.nodes.iter().find(|node| node.node_id == current) {
        let Some(parent_id) = node.parent_id.as_deref() else {
            break;
        };
        if !active_path.insert(parent_id) {
            break;
        }
        current = parent_id;
    }

    active_path
}

fn pretty_marker(node: &SpineTreeNode, active: bool, has_children: bool) -> Span<'static> {
    match pretty_marker_text(node, active, has_children) {
        "◉" => "◉".cyan().bold(),
        "✓" => "✓".green().bold(),
        "×" => "×".red().bold(),
        "!" => "!".yellow().bold(),
        "▾" => "▾".dim(),
        "◌" => "◌".dim(),
        marker => Span::from(marker),
    }
}

fn pretty_marker_text(node: &SpineTreeNode, active: bool, has_children: bool) -> &'static str {
    if active {
        return "◉";
    }
    match node.spawn_outcome {
        Some(SpineSpawnOutcome::Completed) => return "✓",
        Some(SpineSpawnOutcome::Errored) => return "×",
        Some(SpineSpawnOutcome::Aborted) => return "!",
        None => {}
    }
    match node.status {
        SpineTreeNodeStatus::Live => "◉",
        SpineTreeNodeStatus::Closed => "✓",
        SpineTreeNodeStatus::Compacted => "◌",
        SpineTreeNodeStatus::Opened if has_children => "▾",
        SpineTreeNodeStatus::Opened => "◌",
    }
}

fn render_history_bucket(
    count: usize,
    prefix: &str,
    is_last: bool,
    width: u16,
    out: &mut Vec<Line<'static>>,
) {
    let line_prefix = format!("{}{}", prefix, pretty_branch(is_last));
    let child_prefix = format!("{}{}", prefix, pretty_child_prefix(is_last));
    let line = Line::from(vec![
        Span::from(line_prefix).dim(),
        "◌".dim(),
        " ".into(),
        Span::from(history_bucket_label(count)).dim(),
    ]);
    let wrapped = adaptive_wrap_line(
        &line,
        RtOptions::new(width.saturating_sub(2).max(1) as usize)
            .subsequent_indent(Span::from(format!("{child_prefix}  ")).dim().into()),
    );
    push_owned_lines(&wrapped, out);
}

fn history_bucket_label(count: usize) -> String {
    if count == 1 {
        "1 previous task".to_string()
    } else {
        format!("{count} previous tasks")
    }
}

fn pretty_node_label_text(node: &SpineTreeNode, active: bool) -> String {
    trimmed_summary(node)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| pretty_default_node_label(node, active).to_string())
}

fn trimmed_summary(node: &SpineTreeNode) -> Option<&str> {
    node.summary
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn should_elide_pretty_node(node: &SpineTreeNode, has_children: bool, active: bool) -> bool {
    node.kind == SpineTreeNodeKind::RootEpoch
        || (has_children && !active && trimmed_summary(node).is_none())
}

fn pretty_default_node_label(node: &SpineTreeNode, active: bool) -> &'static str {
    if active || node.status == SpineTreeNodeStatus::Live {
        return "Current task";
    }
    match node.status {
        SpineTreeNodeStatus::Live => "Current task",
        SpineTreeNodeStatus::Opened => "Task",
        SpineTreeNodeStatus::Closed => "Completed task",
        SpineTreeNodeStatus::Compacted => "Previous task",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpineTreeSnapshotValidationError {
    DuplicateNodeId,
    MissingActiveNode,
    MissingParent,
    ParentCycle,
}

impl SpineTreeSnapshotValidationError {
    fn label(self) -> &'static str {
        match self {
            SpineTreeSnapshotValidationError::DuplicateNodeId => "duplicate node id",
            SpineTreeSnapshotValidationError::MissingActiveNode => "missing active node",
            SpineTreeSnapshotValidationError::MissingParent => "missing parent node",
            SpineTreeSnapshotValidationError::ParentCycle => "parent cycle",
        }
    }
}

fn validate_spine_tree_snapshot(
    snapshot: &SpineTreeUpdatedNotification,
) -> Result<(), SpineTreeSnapshotValidationError> {
    if snapshot.nodes.is_empty() {
        return Ok(());
    }

    let mut node_ids = HashSet::new();
    for node in &snapshot.nodes {
        if !node_ids.insert(node.node_id.as_str()) {
            return Err(SpineTreeSnapshotValidationError::DuplicateNodeId);
        }
    }

    if !node_ids.contains(snapshot.active_node_id.as_str()) {
        return Err(SpineTreeSnapshotValidationError::MissingActiveNode);
    }

    for node in &snapshot.nodes {
        if let Some(parent_id) = node.parent_id.as_deref()
            && !node_ids.contains(parent_id)
        {
            return Err(SpineTreeSnapshotValidationError::MissingParent);
        }
    }

    for node in &snapshot.nodes {
        let mut seen = HashSet::new();
        let mut current_id = Some(node.node_id.as_str());
        while let Some(node_id) = current_id {
            if !seen.insert(node_id) {
                return Err(SpineTreeSnapshotValidationError::ParentCycle);
            }
            current_id = snapshot
                .nodes
                .iter()
                .find(|candidate| candidate.node_id == node_id)
                .and_then(|candidate| candidate.parent_id.as_deref());
        }
    }

    Ok(())
}

fn invalid_snapshot_display_line(error: SpineTreeSnapshotValidationError) -> Line<'static> {
    vec![
        format!("  {}", pretty_branch(true)).dim(),
        Span::from(invalid_snapshot_message(error)).red().bold(),
    ]
    .into()
}

fn invalid_snapshot_raw_line(error: SpineTreeSnapshotValidationError) -> Line<'static> {
    Line::from(format!(
        "  {}{}",
        pretty_branch(true),
        invalid_snapshot_message(error)
    ))
}

fn invalid_snapshot_message(error: SpineTreeSnapshotValidationError) -> String {
    format!("{INVALID_SPINE_TREE_SNAPSHOT_LABEL}: {}", error.label())
}

fn child_nodes<'a>(
    snapshot: &'a SpineTreeUpdatedNotification,
    parent_id: Option<&str>,
) -> Vec<&'a SpineTreeNode> {
    snapshot
        .nodes
        .iter()
        .filter(|node| node.parent_id.as_deref() == parent_id)
        .collect()
}

fn pretty_branch(is_last: bool) -> &'static str {
    if is_last { "└ " } else { "├ " }
}

fn pretty_child_prefix(is_last: bool) -> &'static str {
    if is_last { "  " } else { "│ " }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn snapshot(active_node_id: &str, nodes: Vec<SpineTreeNode>) -> SpineTreeUpdatedNotification {
        SpineTreeUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            snapshot_seq: 1,
            active_node_id: active_node_id.to_string(),
            nodes,
            settled_spawn_call_ids: Vec::new(),
        }
    }

    fn node(
        node_id: &str,
        parent_id: Option<&str>,
        summary: Option<&str>,
        status: SpineTreeNodeStatus,
    ) -> SpineTreeNode {
        SpineTreeNode {
            node_id: node_id.to_string(),
            parent_id: parent_id.map(str::to_string),
            kind: SpineTreeNodeKind::Task,
            status,
            summary: summary.map(str::to_string),
            memory_summary: None,
            start: 0,
            end: None,
            context_pressure: None,
            spawn_outcome: None,
        }
    }

    fn root_epoch(
        node_id: &str,
        summary: Option<&str>,
        status: SpineTreeNodeStatus,
    ) -> SpineTreeNode {
        let mut node = node(node_id, None, summary, status);
        node.kind = SpineTreeNodeKind::RootEpoch;
        node
    }

    #[test]
    fn renders_pretty_hierarchy_and_active_path() {
        let cell = new_spine_tree_snapshot(snapshot(
            "2.1",
            vec![
                node("1", None, Some("earlier work"), SpineTreeNodeStatus::Closed),
                node(
                    "2",
                    None,
                    Some("current scope"),
                    SpineTreeNodeStatus::Opened,
                ),
                node(
                    "2.1",
                    Some("2"),
                    Some("focused task"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        ));

        insta::assert_snapshot!(render(&cell.display_lines(80)), @r###"
        • Spine Tree
          ├ ✓ earlier work
          └ ▾ current scope
            └ ◉ focused task
        "###);
    }

    #[test]
    fn renders_pretty_header_in_green_bold() {
        let header = pretty_header(&snapshot(
            "1",
            vec![node(
                "1",
                None,
                Some("current task"),
                SpineTreeNodeStatus::Live,
            )],
        ));
        let title = &header.spans[1];

        assert_eq!(title.content.as_ref(), "Spine Tree");
        assert_eq!(title.style.fg, Some(Color::Green));
        assert!(title.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn folds_older_siblings_and_elides_empty_structural_nodes() {
        let cell = new_spine_tree_snapshot(snapshot(
            "3.3",
            vec![
                node("1", None, Some("old root 1"), SpineTreeNodeStatus::Closed),
                node("2", None, Some("old root 2"), SpineTreeNodeStatus::Closed),
                node("3", None, None, SpineTreeNodeStatus::Opened),
                node(
                    "3.1",
                    Some("3"),
                    Some("child 1"),
                    SpineTreeNodeStatus::Closed,
                ),
                node(
                    "3.2",
                    Some("3"),
                    Some("child 2"),
                    SpineTreeNodeStatus::Closed,
                ),
                node(
                    "3.3",
                    Some("3"),
                    Some("active child"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        ));

        let lines = cell.display_lines(80);
        let rendered = render(&lines);
        insta::assert_snapshot!(rendered, @r###"
        • Spine Tree
          ├ ◌ 2 previous tasks
          ├ ✓ child 1
          ├ ✓ child 2
          └ ◉ active child
        "###);
        let history_label = lines[1]
            .spans
            .iter()
            .find(|span| span.content.contains("previous tasks"))
            .expect("history bucket label");
        assert!(history_label.style.add_modifier.contains(Modifier::DIM));
        assert!(!rendered.contains("old root"));
        assert!(!rendered.contains("3 "));
    }

    #[test]
    fn hides_root_epochs_and_promotes_their_tasks_in_display_and_raw() {
        let cell = new_spine_tree_snapshot(snapshot(
            "3.2",
            vec![
                root_epoch("1", Some("root"), SpineTreeNodeStatus::Closed),
                node(
                    "1.1",
                    Some("1"),
                    Some("first task"),
                    SpineTreeNodeStatus::Closed,
                ),
                root_epoch("2", Some("root"), SpineTreeNodeStatus::Closed),
                node(
                    "2.1",
                    Some("2"),
                    Some("second task"),
                    SpineTreeNodeStatus::Closed,
                ),
                root_epoch("3", Some("root"), SpineTreeNodeStatus::Opened),
                node(
                    "3.1",
                    Some("3"),
                    Some("current scope"),
                    SpineTreeNodeStatus::Opened,
                ),
                node(
                    "3.2",
                    Some("3.1"),
                    Some("active task"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        ));

        let display = render(&cell.display_lines(80));
        insta::assert_snapshot!(display, @r###"
        • Spine Tree
          ├ ✓ first task
          ├ ✓ second task
          └ ▾ current scope
            └ ◉ active task
        "###);
        assert!(!display.contains("root"));

        let raw = render(&cell.raw_lines());
        assert!(!raw.contains("root"));
        assert!(raw.contains("first task"));
        assert!(raw.contains("active task"));
    }

    #[test]
    fn root_epoch_only_snapshot_renders_empty_pretty_tree() {
        let cell = new_spine_tree_snapshot(snapshot(
            "1",
            vec![root_epoch("1", Some("root"), SpineTreeNodeStatus::Live)],
        ));

        insta::assert_snapshot!(render(&cell.display_lines(80)), @r###"
        • Spine Tree
          └ (empty)
        "###);
        insta::assert_snapshot!(render(&cell.raw_lines()), @r###"
        Spine Tree
          └ (empty)
        "###);
    }

    #[test]
    fn debug_tree_keeps_root_epoch_structure() {
        let cell = new_debug_spine_tree_snapshot(snapshot(
            "1",
            vec![root_epoch("1", Some("root"), SpineTreeNodeStatus::Live)],
        ));

        let rendered = render(&cell.display_lines(80));
        assert!(rendered.contains("Debug Spine Tree"));
        assert!(rendered.contains("1 root current"));
    }

    #[test]
    fn wraps_long_summary_using_tree_indent() {
        let cell = new_spine_tree_snapshot(snapshot(
            "1",
            vec![node(
                "1",
                None,
                Some("a summary that is deliberately long enough to wrap"),
                SpineTreeNodeStatus::Live,
            )],
        ));

        let lines = cell.display_lines(24);
        assert!(lines.len() > 2);
        assert!(render(&lines).contains("  └ ◉ "));
        assert!(
            lines[2].spans[0].style.add_modifier.contains(Modifier::DIM),
            "wrapped tree prefix should retain the tree line style: {lines:?}"
        );
    }

    #[test]
    fn reports_invalid_parent_snapshot_without_panicking() {
        let cell = new_spine_tree_snapshot(snapshot(
            "1",
            vec![SpineTreeNode {
                node_id: "1".to_string(),
                parent_id: Some("missing".to_string()),
                kind: SpineTreeNodeKind::Task,
                status: SpineTreeNodeStatus::Live,
                summary: None,
                memory_summary: None,
                start: 0,
                end: None,
                context_pressure: None,
                spawn_outcome: None,
            }],
        ));

        assert!(
            render(&cell.display_lines(80))
                .contains("invalid Spine tree snapshot: missing parent node")
        );
    }

    #[test]
    fn spawn_outcome_controls_the_final_closed_leaf_marker() {
        let mut completed = node(
            "1.1",
            Some("1"),
            Some("completed"),
            SpineTreeNodeStatus::Closed,
        );
        completed.spawn_outcome = Some(SpineSpawnOutcome::Completed);
        let mut errored = completed.clone();
        errored.node_id = "1.2".to_string();
        errored.spawn_outcome = Some(SpineSpawnOutcome::Errored);
        let mut aborted = completed.clone();
        aborted.node_id = "1.3".to_string();
        aborted.spawn_outcome = Some(SpineSpawnOutcome::Aborted);

        assert_eq!(pretty_marker_text(&completed, false, false), "✓");
        assert_eq!(pretty_marker_text(&errored, false, false), "×");
        assert_eq!(pretty_marker_text(&aborted, false, false), "!");
    }

    #[test]
    fn active_root_epoch_promotes_spawn_overlay_into_visible_forest() {
        for closed_children in [false, true] {
            let mut root = node("root", None, None, SpineTreeNodeStatus::Live);
            root.kind = SpineTreeNodeKind::RootEpoch;
            let mut nodes = vec![root];
            if closed_children {
                nodes.push(node(
                    "root.1",
                    Some("root"),
                    Some("previous work"),
                    SpineTreeNodeStatus::Closed,
                ));
            }
            let mut state = SpineTreeViewState::default();
            state.apply_tree_update(snapshot("root", nodes));
            state.apply_spawn_progress(SpineSpawnProgressUpdatedNotification {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                call_id: "spawn-root".to_string(),
                tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                    ordinal: 0,
                    summary: "root worker".to_string(),
                    thread_id: "child-root".to_string(),
                    agent_path: Some("/root/worker".to_string()),
                    status: codex_app_server_protocol::CollabAgentStatus::Running,
                }],
            });

            let rendered = render(
                &state
                    .render_cell()
                    .expect("tree snapshot should render")
                    .display_lines(80),
            );
            assert!(rendered.contains("root worker"), "{rendered}");
            assert!(!rendered.contains("leaf 0"), "{rendered}");
            assert!(rendered.contains("Waiting for activity..."), "{rendered}");
            if closed_children {
                assert!(rendered.contains("previous work"), "{rendered}");
            } else {
                assert!(!rendered.contains("(empty)"), "{rendered}");
            }
        }
    }

    #[test]
    fn live_spawn_overlay_ticks_while_snapshot_copy_stays_static() {
        let mut state = SpineTreeViewState::new(true);
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("active"), SpineTreeNodeStatus::Live)],
        ));
        state.apply_spawn_progress(SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "spawn-1".to_string(),
            tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "animated worker".to_string(),
                thread_id: "child-1".to_string(),
                agent_path: Some("/root/worker".to_string()),
                status: codex_app_server_protocol::CollabAgentStatus::PendingInit,
            }],
        });

        let live = state.render_cell().expect("live tree should render");
        let snapshot = state.snapshot_cell().expect("snapshot should render");

        assert!(live.transcript_animation_tick().is_some());
        assert_eq!(snapshot.transcript_animation_tick(), None);
        assert!(
            live.display_lines(80)
                .iter()
                .any(|line| line.to_string().contains("animated worker"))
        );
        assert!(
            snapshot
                .display_lines(80)
                .iter()
                .all(|line| !line.to_string().contains("animated worker"))
        );
    }

    #[test]
    fn live_tail_highlights_only_the_active_node() {
        let mut state = SpineTreeViewState::new(false);
        state.apply_tree_update(snapshot(
            "1.1",
            vec![
                node(
                    "1",
                    None,
                    Some("static parent"),
                    SpineTreeNodeStatus::Opened,
                ),
                node(
                    "1.1",
                    Some("1"),
                    Some("working summary"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        ));
        state.apply_spawn_progress(SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "spawn-1".to_string(),
            tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "child".to_string(),
                thread_id: "child-1".to_string(),
                agent_path: Some("/root/child".to_string()),
                status: codex_app_server_protocol::CollabAgentStatus::PendingInit,
            }],
        });

        let live = state.render_cell().expect("live tree should render");
        let lines = live.display_lines(80);
        let active_line = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("working summary"))
            })
            .expect("active node line");
        let marker = active_line
            .spans
            .iter()
            .find(|span| span.content == "◉")
            .expect("active marker");
        let summary = active_line
            .spans
            .iter()
            .find(|span| span.content.contains("working summary"))
            .expect("active summary");

        assert_eq!(marker.style.fg, Some(Color::Green));
        assert!(!marker.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(summary.style.fg, None);

        let snapshot = state.snapshot_cell().expect("snapshot should render");
        let static_lines = snapshot.display_lines(80);
        let static_active = static_lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("working summary"))
            })
            .expect("static active node line");
        let static_marker = static_active
            .spans
            .iter()
            .find(|span| span.content == "◉")
            .expect("static active marker");

        assert_eq!(static_marker.style.fg, Some(Color::Cyan));
        assert!(static_marker.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn semantic_snapshot_alone_does_not_create_a_live_tail() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("active"), SpineTreeNodeStatus::Live)],
        ));

        assert!(state.render_cell().is_none());
        assert!(state.snapshot_cell().is_some());
    }

    #[test]
    fn tree_commit_removes_only_the_settled_spawn_overlays() {
        let progress = |call_id: &str, agent_path: &str| SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: call_id.to_string(),
            tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                ordinal: 0,
                summary: format!("task for {call_id}"),
                thread_id: format!("child-{call_id}"),
                agent_path: Some(agent_path.to_string()),
                status: codex_app_server_protocol::CollabAgentStatus::Running,
            }],
        };
        let mut state = SpineTreeViewState::default();
        state.apply_spawn_progress(progress("spawn-1", "/root/first"));
        state.apply_spawn_progress(progress("spawn-2", "/root/second"));

        let mut committed = snapshot(
            "1",
            vec![node("1", None, Some("active"), SpineTreeNodeStatus::Live)],
        );
        committed.settled_spawn_call_ids = vec!["spawn-1".to_string()];
        state.apply_tree_update(committed.clone());
        state.apply_tree_update(committed.clone());

        assert!(!state.has_spawn_call("spawn-1"));
        assert!(state.has_spawn_call("spawn-2"));
        assert!(state.render_cell().is_some());

        committed.snapshot_seq += 1;
        committed.settled_spawn_call_ids = vec!["spawn-2".to_string()];
        state.apply_tree_update(committed.clone());
        assert!(state.render_cell().is_none());
        state.apply_tree_update(committed);
    }

    #[test]
    fn settled_spawn_progress_cannot_recreate_a_transient_overlay() {
        let progress = SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "spawn-settled".to_string(),
            tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "completed worker".to_string(),
                thread_id: "child-settled".to_string(),
                agent_path: Some("/root/completed-worker".to_string()),
                status: codex_app_server_protocol::CollabAgentStatus::Completed,
            }],
        };
        let mut state = SpineTreeViewState::default();
        state.apply_spawn_progress(progress.clone());
        assert!(state.has_spawn_call("spawn-settled"));

        let mut committed = snapshot(
            "1",
            vec![node(
                "1",
                None,
                Some("completed worker"),
                SpineTreeNodeStatus::Closed,
            )],
        );
        committed.settled_spawn_call_ids = vec!["spawn-settled".to_string()];
        state.apply_tree_update(committed);
        assert!(!state.has_spawn_call("spawn-settled"));

        state.apply_spawn_progress(progress);
        assert!(!state.has_spawn_call("spawn-settled"));
        assert!(state.render_cell().is_none());
    }

    #[test]
    fn settled_spawn_call_ignores_progress_that_arrives_after_the_tree_commit() {
        let mut state = SpineTreeViewState::default();
        let mut committed = snapshot(
            "1",
            vec![node(
                "1",
                None,
                Some("completed worker"),
                SpineTreeNodeStatus::Closed,
            )],
        );
        committed.settled_spawn_call_ids = vec!["spawn-settled".to_string()];
        state.apply_tree_update(committed);

        state.apply_spawn_progress(SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "spawn-settled".to_string(),
            tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "completed worker".to_string(),
                thread_id: "child-settled".to_string(),
                agent_path: Some("/root/completed-worker".to_string()),
                status: codex_app_server_protocol::CollabAgentStatus::Completed,
            }],
        });

        assert!(!state.has_spawn_call("spawn-settled"));
        assert!(state.render_cell().is_none());
    }

    #[test]
    fn stale_tree_update_cannot_replace_snapshot_or_settle_overlay() {
        let mut state = SpineTreeViewState::default();
        let mut current = snapshot(
            "1",
            vec![node("1", None, Some("current"), SpineTreeNodeStatus::Live)],
        );
        current.snapshot_seq = 2;
        state.apply_tree_update(current);
        state.apply_spawn_progress(SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn-2".to_string(),
            call_id: "spawn-live".to_string(),
            tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "worker".to_string(),
                thread_id: "child-live".to_string(),
                agent_path: Some("/root/worker".to_string()),
                status: codex_app_server_protocol::CollabAgentStatus::Running,
            }],
        });

        let mut stale = snapshot(
            "1",
            vec![node("1", None, Some("stale"), SpineTreeNodeStatus::Live)],
        );
        stale.settled_spawn_call_ids = vec!["spawn-live".to_string()];
        state.apply_tree_update(stale);

        assert_eq!(
            state.snapshot().map(|snapshot| snapshot.snapshot_seq),
            Some(2)
        );
        assert!(state.has_spawn_call("spawn-live"));
    }

    #[test]
    fn activity_update_targets_one_turn_and_call() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("active"), SpineTreeNodeStatus::Live)],
        ));
        for (turn_id, call_id, summary) in [
            ("turn-1", "spawn-1", "first worker"),
            ("turn-2", "spawn-2", "second worker"),
        ] {
            state.apply_spawn_progress(SpineSpawnProgressUpdatedNotification {
                thread_id: "thread".to_string(),
                turn_id: turn_id.to_string(),
                call_id: call_id.to_string(),
                tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                    ordinal: 0,
                    summary: summary.to_string(),
                    thread_id: format!("child-{call_id}"),
                    agent_path: Some("/root/shared".to_string()),
                    status: codex_app_server_protocol::CollabAgentStatus::Running,
                }],
            });
        }

        assert!(state.apply_activity(
            "turn-2",
            "spawn-2",
            "child-spawn-2",
            &ServerNotification::AgentMessageDelta(
                codex_app_server_protocol::AgentMessageDeltaNotification {
                    thread_id: "child".to_string(),
                    turn_id: "child-turn".to_string(),
                    item_id: "message".to_string(),
                    delta: "second only".to_string(),
                },
            ),
            None,
        ));

        let rendered = render(
            &state
                .render_cell()
                .expect("tree state should render")
                .display_lines(80),
        );
        assert_eq!(rendered.matches("second only").count(), 1, "{rendered}");
        assert!(rendered.contains("Waiting for activity..."), "{rendered}");
    }

    #[test]
    fn mounts_spawn_overlay_under_the_active_node() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1.1",
            vec![
                node("1", None, Some("parent"), SpineTreeNodeStatus::Opened),
                node("1.1", Some("1"), Some("active"), SpineTreeNodeStatus::Live),
            ],
        ));
        state.apply_spawn_progress(SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "spawn-1".to_string(),
            tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "inspect events".to_string(),
                thread_id: "child-inspector".to_string(),
                agent_path: Some("/root/inspector".to_string()),
                status: codex_app_server_protocol::CollabAgentStatus::Running,
            }],
        });
        let cell = state.render_cell().expect("tree snapshot should render");

        let rendered = render(&cell.display_lines(80));
        let task_line = rendered
            .lines()
            .find(|line| line.contains("inspect events"))
            .expect("spawn task line should render");
        assert!(task_line.starts_with("      └ "), "{rendered}");
        assert!(!task_line.contains("leaf 0"), "{rendered}");
        assert!(!rendered.contains("spine.spawn"));
        assert!(!rendered.contains("/root/inspector"));
    }

    #[test]
    fn multiple_spawn_overlays_share_direct_task_sibling_branches() {
        let spawn_progress = |call_id: &str, summary: &str, agent_path: &str| {
            SpineSpawnProgressUpdatedNotification {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                call_id: call_id.to_string(),
                tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                    ordinal: 0,
                    summary: summary.to_string(),
                    thread_id: format!("child-{call_id}"),
                    agent_path: Some(agent_path.to_string()),
                    status: codex_app_server_protocol::CollabAgentStatus::Running,
                }],
            }
        };
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1.1",
            vec![
                node("1", None, Some("parent"), SpineTreeNodeStatus::Opened),
                node("1.1", Some("1"), Some("active"), SpineTreeNodeStatus::Live),
            ],
        ));
        state.apply_spawn_progress(spawn_progress("spawn-1", "first task", "/root/first"));
        state.apply_spawn_progress(spawn_progress("spawn-2", "second task", "/root/second"));
        let cell = state.render_cell().expect("tree snapshot should render");

        let task_lines = cell
            .display_lines(80)
            .into_iter()
            .map(|line| line.to_string())
            .filter(|line| line.contains("first task") || line.contains("second task"))
            .collect::<Vec<_>>();
        assert_eq!(task_lines.len(), 2, "{task_lines:?}");
        assert!(task_lines[0].starts_with("      ├ "), "{task_lines:?}");
        assert!(task_lines[1].starts_with("      └ "), "{task_lines:?}");
    }
}
