use super::spine_spawn_completion::SettledTaskVisual;
use super::spine_spawn_completion::plan_handoff;
use super::spine_spawn_progress::SpineSpawnOverlay;
use super::*;
use crate::product_brand::SPINE_BRAND_COLOR;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SpineSpawnOutcome;
use codex_app_server_protocol::SpineSpawnProgressUpdatedNotification;
use codex_app_server_protocol::SpineTreeNode;
use codex_app_server_protocol::SpineTreeNodeKind;
use codex_app_server_protocol::SpineTreeNodeStatus;
use codex_app_server_protocol::SpineTreeUpdatedNotification;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

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
        pending_handoff: None,
        animations_enabled: false,
        automatic_history: false,
    }
}

pub(crate) fn new_debug_spine_tree_snapshot(
    snapshot: SpineTreeUpdatedNotification,
) -> SpineTreeUpdateCell {
    SpineTreeUpdateCell {
        snapshot,
        display_mode: SpineTreeDisplayMode::Debug(None),
        spawn_overlays: Vec::new(),
        pending_handoff: None,
        animations_enabled: false,
        automatic_history: false,
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
        pending_handoff: None,
        animations_enabled: false,
        automatic_history: false,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SpineTreeViewState {
    snapshot: Option<SpineTreeUpdatedNotification>,
    pending_history: Option<SpineTreeUpdatedNotification>,
    overlays: Vec<SpineSpawnOverlay>,
    settled_spawn_call_ids: HashSet<String>,
    pending_handoff: Option<PendingTreeHandoff>,
    animations_enabled: bool,
}

#[derive(Debug, Clone)]
struct PendingTreeHandoff {
    snapshot: SpineTreeUpdatedNotification,
    reveal_at: Instant,
    overlays: Vec<SpineSpawnOverlay>,
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
            pending_history: None,
            overlays: Vec::new(),
            settled_spawn_call_ids: HashSet::new(),
            pending_handoff: None,
            animations_enabled,
        }
    }

    pub(crate) fn snapshot(&self) -> Option<&SpineTreeUpdatedNotification> {
        self.snapshot.as_ref()
    }

    pub(crate) fn apply_tree_update(&mut self, snapshot: SpineTreeUpdatedNotification) {
        self.apply_tree_update_at(snapshot, Instant::now());
    }

    fn apply_tree_update_at(&mut self, snapshot: SpineTreeUpdatedNotification, now: Instant) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| snapshot.snapshot_seq < current.snapshot_seq)
        {
            return;
        }
        let newly_settled = snapshot
            .settled_spawn_call_ids
            .iter()
            .filter(|call_id| !self.settled_spawn_call_ids.contains(call_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let display_changed = self
            .snapshot
            .as_ref()
            .is_some_and(|current| display_tree_changed(current, &snapshot));
        let handoff_superseded =
            self.pending_handoff.is_some() && (!newly_settled.is_empty() || display_changed);
        if handoff_superseded {
            self.pending_handoff = None;
        }
        let prior = self.snapshot.replace(snapshot);
        self.settled_spawn_call_ids
            .extend(newly_settled.iter().cloned());

        let mut started_handoff = false;
        if !newly_settled.is_empty()
            && let (true, Some(prior), Some(settled_tasks), Some(latest)) = (
                self.animations_enabled,
                prior,
                self.settled_visuals_for(&newly_settled),
                self.snapshot.as_ref(),
            )
            && let Some(reveal_at) = plan_handoff(&prior, latest, &settled_tasks, now)
        {
            self.pending_handoff = Some(PendingTreeHandoff {
                snapshot: prior,
                reveal_at,
                overlays: self
                    .overlays
                    .iter()
                    .filter(|overlay| {
                        newly_settled
                            .iter()
                            .any(|call_id| call_id == overlay.call_id())
                    })
                    .cloned()
                    .collect(),
            });
            started_handoff = true;
        }
        self.overlays
            .retain(|overlay| !self.settled_spawn_call_ids.contains(overlay.call_id()));

        let refresh_pending_history = !started_handoff && (display_changed || handoff_superseded);
        if refresh_pending_history {
            self.pending_history = self.snapshot.clone();
        }
    }

    fn settled_visuals_for(&self, call_ids: &[String]) -> Option<Vec<SettledTaskVisual>> {
        let mut seen = HashSet::with_capacity(call_ids.len());
        let mut tasks = Vec::new();
        for call_id in call_ids {
            seen.insert(call_id.as_str()).then_some(())?;
            let mut overlays = self
                .overlays
                .iter()
                .filter(|overlay| overlay.call_id() == call_id);
            let overlay = overlays.next()?;
            if overlays.next().is_some() {
                return None;
            }
            tasks.extend(overlay.settled_task_visuals()?);
        }
        Some(tasks)
    }

    pub(crate) fn clear_incomplete_spawn_overlays(&mut self, turn_id: Option<&str>) -> bool {
        let pending_cleared = self
            .pending_handoff
            .take_if(|pending| {
                pending
                    .overlays
                    .iter()
                    .any(|overlay| turn_id.is_none_or(|turn_id| overlay.turn_id() == turn_id))
            })
            .is_some();
        let before = self.overlays.len();
        self.overlays
            .retain(|overlay| turn_id.is_some_and(|turn_id| overlay.turn_id() != turn_id));
        if pending_cleared {
            self.pending_history = self.snapshot.clone();
        }
        pending_cleared || self.overlays.len() != before
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
        if self.overlays.is_empty() && self.pending_handoff.is_none() {
            return None;
        }
        let snapshot = self.snapshot.clone()?;
        Some(SpineTreeUpdateCell {
            snapshot,
            display_mode: SpineTreeDisplayMode::Pretty,
            spawn_overlays: self.overlays.clone(),
            pending_handoff: self.pending_handoff.clone(),
            animations_enabled: self.animations_enabled,
            automatic_history: false,
        })
    }

    pub(crate) fn snapshot_cell(&self) -> Option<SpineTreeUpdateCell> {
        let snapshot = self.snapshot.clone()?;
        Some(SpineTreeUpdateCell {
            snapshot,
            display_mode: SpineTreeDisplayMode::Pretty,
            spawn_overlays: Vec::new(),
            pending_handoff: None,
            animations_enabled: false,
            automatic_history: false,
        })
    }

    pub(crate) fn take_pending_history_cell(&mut self) -> Option<SpineTreeUpdateCell> {
        self.pending_history
            .take()
            .map(SpineTreeUpdateCell::automatic_history)
    }

    #[cfg(test)]
    pub(crate) fn has_pending_history(&self) -> bool {
        self.pending_history.is_some()
    }

    pub(crate) fn promote_due_handoff_to_pending(&mut self, now: Instant) -> bool {
        if self
            .pending_handoff
            .as_ref()
            .is_none_or(|pending| now < pending.reveal_at)
        {
            return false;
        }
        self.pending_handoff = None;
        self.pending_history = self.snapshot.clone();
        true
    }

    #[cfg(test)]
    pub(crate) fn make_pending_handoff_due(&mut self) {
        if let Some(handoff) = self.pending_handoff.as_mut() {
            handoff.reveal_at = Instant::now();
        }
    }

    pub(crate) fn take_due_handoff_history(&mut self, now: Instant) -> Option<SpineTreeUpdateCell> {
        if !self.promote_due_handoff_to_pending(now) {
            return None;
        }
        self.take_pending_history_cell()
    }

    #[cfg(test)]
    pub(crate) fn has_spawn_call(&self, call_id: &str) -> bool {
        self.overlays
            .iter()
            .any(|overlay| overlay.call_id() == call_id)
    }
}

fn display_tree_changed(
    previous: &SpineTreeUpdatedNotification,
    current: &SpineTreeUpdatedNotification,
) -> bool {
    previous.active_node_id != current.active_node_id
        || previous.nodes.len() != current.nodes.len()
        || previous
            .nodes
            .iter()
            .zip(&current.nodes)
            .any(|(left, right)| {
                left.node_id != right.node_id
                    || left.parent_id != right.parent_id
                    || left.kind != right.kind
                    || left.status != right.status
                    || left.summary != right.summary
                    || left.memory_summary != right.memory_summary
                    || left.spawn_outcome != right.spawn_outcome
                    || left.start != right.start
                    || left.end != right.end
            })
}

#[derive(Debug, Clone)]
pub(crate) struct SpineTreeUpdateCell {
    snapshot: SpineTreeUpdatedNotification,
    display_mode: SpineTreeDisplayMode,
    spawn_overlays: Vec<SpineSpawnOverlay>,
    pending_handoff: Option<PendingTreeHandoff>,
    animations_enabled: bool,
    automatic_history: bool,
}

#[derive(Debug, Clone)]
enum SpineTreeDisplayMode {
    Pretty,
    Debug(Option<String>),
}

impl HistoryCell for SpineTreeUpdateCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines_at(width, Instant::now())
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
        let now = Instant::now();
        let started_at = self
            .spawn_overlays
            .iter()
            .chain(
                self.pending_handoff
                    .iter()
                    .filter(|pending| now < pending.reveal_at)
                    .flat_map(|pending| &pending.overlays),
            )
            .map(SpineSpawnOverlay::animation_start)
            .min()?;
        Some(now.saturating_duration_since(started_at).as_millis() as u64 / 50)
    }
}

impl SpineTreeUpdateCell {
    fn automatic_history(snapshot: SpineTreeUpdatedNotification) -> Self {
        Self {
            snapshot,
            display_mode: SpineTreeDisplayMode::Pretty,
            spawn_overlays: Vec::new(),
            pending_handoff: None,
            animations_enabled: false,
            automatic_history: true,
        }
    }

    pub(crate) fn turn_id(&self) -> &str {
        &self.snapshot.turn_id
    }

    pub(crate) fn snapshot_seq(&self) -> u64 {
        self.snapshot.snapshot_seq
    }

    pub(crate) fn is_automatic_history(&self) -> bool {
        self.automatic_history
    }

    pub(crate) fn next_frame_in(&self, now: Instant) -> Option<Duration> {
        if !self.animations_enabled || !matches!(self.display_mode, SpineTreeDisplayMode::Pretty) {
            return None;
        }
        let pending = self
            .pending_handoff
            .as_ref()
            .filter(|handoff| now < handoff.reveal_at);
        self.spawn_overlays
            .iter()
            .chain(pending.into_iter().flat_map(|pending| &pending.overlays))
            .filter_map(|overlay| overlay.next_completion_frame_in(now))
            .chain(pending.map(|handoff| handoff.reveal_at - now))
            .min()
    }

    fn display_lines_at(&self, width: u16, now: Instant) -> Vec<Line<'static>> {
        match &self.display_mode {
            SpineTreeDisplayMode::Pretty => {
                let active_handoff = self
                    .pending_handoff
                    .as_ref()
                    .filter(|pending| now < pending.reveal_at);
                let mut overlays = self.spawn_overlays.clone();
                if let Some(pending) = active_handoff {
                    overlays.extend_from_slice(&pending.overlays);
                }
                pretty_display_lines(
                    active_handoff.map_or(&self.snapshot, |pending| &pending.snapshot),
                    &overlays,
                    width,
                    self.animations_enabled,
                )
            }
            SpineTreeDisplayMode::Debug(node_id) => {
                debug::display_lines(&self.snapshot, width, node_id.as_deref())
            }
        }
    }
}

fn pretty_display_lines(
    snapshot: &SpineTreeUpdatedNotification,
    overlays: &[SpineSpawnOverlay],
    width: u16,
    animations_enabled: bool,
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
    vec!["• ".dim(), "Spine Tree".fg(SPINE_BRAND_COLOR).bold()].into()
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
) {
    let children = child_nodes(snapshot, Some(node.node_id.as_str()));
    let active = node.node_id == snapshot.active_node_id;
    let line_prefix = format!("{}{}", prefix, pretty_branch(is_last));
    let child_prefix = format!("{}{}", prefix, pretty_child_prefix(is_last));
    let mut spans = vec![Span::from(line_prefix).dim()];
    spans.push(pretty_marker(node, active, !children.is_empty()));
    spans.push(" ".into());
    spans.push(Span::from(pretty_node_label_text(node, active)));

    let line = Line::from(spans);
    let wrapped = adaptive_wrap_line(
        &line,
        RtOptions::new(width.saturating_sub(2).max(1) as usize)
            .subsequent_indent(Span::from(format!("{child_prefix}  ")).dim().into()),
    );
    push_owned_lines(&wrapped, out);

    if should_collapse_pretty_subtree(node, !children.is_empty(), active_path) {
        return;
    }

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
                if should_collapse_pretty_subtree(node, !children.is_empty(), active_path) {
                    continue;
                }
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
    is_completed_history_node(node)
        && trimmed_summary(node).is_none()
        && !active_path.contains(node.node_id.as_str())
}

fn should_collapse_pretty_subtree(
    node: &SpineTreeNode,
    has_children: bool,
    active_path: &HashSet<&str>,
) -> bool {
    has_children && is_completed_history_node(node) && !active_path.contains(node.node_id.as_str())
}

fn is_completed_history_node(node: &SpineTreeNode) -> bool {
    matches!(
        node.status,
        SpineTreeNodeStatus::Closed | SpineTreeNodeStatus::Compacted
    )
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
        Span::from(count.to_string()).green(),
        " previous ".green(),
        Span::from(history_bucket_noun(count)).green(),
    ]);
    let wrapped = adaptive_wrap_line(
        &line,
        RtOptions::new(width.saturating_sub(2).max(1) as usize)
            .subsequent_indent(Span::from(format!("{child_prefix}  ")).dim().into()),
    );
    push_owned_lines(&wrapped, out);
}

fn history_bucket_label(count: usize) -> String {
    format!("{count} previous {}", history_bucket_noun(count))
}

fn history_bucket_noun(count: usize) -> &'static str {
    if count == 1 { "leaf" } else { "leaves" }
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
        || (has_children
            && !active
            && trimmed_summary(node).is_none()
            && !is_completed_history_node(node))
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

    fn spawn_progress(
        call_id: &str,
        tasks: &[(&str, &str, codex_app_server_protocol::CollabAgentStatus)],
    ) -> SpineSpawnProgressUpdatedNotification {
        SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: call_id.to_string(),
            tasks: tasks
                .iter()
                .enumerate()
                .map(|(ordinal, (thread_id, summary, status))| {
                    codex_app_server_protocol::SpineSpawnTaskProgress {
                        ordinal: ordinal as u32,
                        summary: (*summary).to_string(),
                        thread_id: (*thread_id).to_string(),
                        agent_path: Some(format!("/root/{thread_id}")),
                        status: status.clone(),
                    }
                })
                .collect(),
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
    fn renders_pretty_header_in_spine_brand_color() {
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
        assert_eq!(title.style.fg, Some(SPINE_BRAND_COLOR));
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
          ├ ◌ 2 previous leaves
          ├ ✓ child 1
          ├ ✓ child 2
          └ ◉ active child
        "###);
        let history_count = lines[1]
            .spans
            .iter()
            .find(|span| span.content == "2")
            .expect("history bucket count");
        assert_eq!(history_count.style.fg, Some(Color::Green));
        assert!(!history_count.style.add_modifier.contains(Modifier::BOLD));
        assert!(!history_count.style.add_modifier.contains(Modifier::DIM));
        let history_previous = lines[1]
            .spans
            .iter()
            .find(|span| span.content == " previous ")
            .expect("history bucket previous label");
        assert_eq!(history_previous.style.fg, Some(Color::Green));
        assert!(!history_previous.style.add_modifier.contains(Modifier::BOLD));
        assert!(!history_previous.style.add_modifier.contains(Modifier::DIM));
        let history_noun = lines[1]
            .spans
            .iter()
            .find(|span| span.content == "leaves")
            .expect("history bucket noun");
        assert_eq!(history_noun.style.fg, Some(Color::Green));
        assert!(!history_noun.style.add_modifier.contains(Modifier::BOLD));
        assert!(!history_noun.style.add_modifier.contains(Modifier::DIM));
        assert!(render(&cell.raw_lines()).contains("2 previous leaves"));
        assert!(!rendered.contains("old root"));
        assert!(!rendered.contains("3 "));
    }

    #[test]
    fn collapses_completed_parent_subtrees_after_root_epoch_promotion() {
        let snapshot = snapshot(
            "2.1",
            vec![
                root_epoch("1", Some("root"), SpineTreeNodeStatus::Compacted),
                node(
                    "1.1",
                    Some("1"),
                    Some("compacted parent"),
                    SpineTreeNodeStatus::Compacted,
                ),
                node(
                    "1.1.1",
                    Some("1.1"),
                    Some("hidden compacted child"),
                    SpineTreeNodeStatus::Closed,
                ),
                node(
                    "1.2",
                    Some("1"),
                    Some("closed parent"),
                    SpineTreeNodeStatus::Closed,
                ),
                node(
                    "1.2.1",
                    Some("1.2"),
                    Some("hidden closed child"),
                    SpineTreeNodeStatus::Closed,
                ),
                root_epoch("2", Some("root"), SpineTreeNodeStatus::Opened),
                node(
                    "2.1",
                    Some("2"),
                    Some("active task"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        );
        let pretty = new_spine_tree_snapshot(snapshot.clone());

        insta::assert_snapshot!(render(&pretty.display_lines(80)), @r###"
        • Spine Tree
          ├ ◌ compacted parent
          ├ ✓ closed parent
          └ ◉ active task
        "###);
        let raw = render(&pretty.raw_lines());
        assert!(!raw.contains("hidden compacted child"), "{raw}");
        assert!(!raw.contains("hidden closed child"), "{raw}");

        let debug = render(&new_debug_spine_tree_snapshot(snapshot).display_lines(80));
        assert!(debug.contains("hidden compacted child"), "{debug}");
        assert!(debug.contains("hidden closed child"), "{debug}");
    }

    #[test]
    fn folds_anonymous_completed_parent_as_one_previous_leaf() {
        let cell = new_spine_tree_snapshot(snapshot(
            "2.1",
            vec![
                root_epoch("1", Some("root"), SpineTreeNodeStatus::Compacted),
                node("1.1", Some("1"), None, SpineTreeNodeStatus::Compacted),
                node(
                    "1.1.1",
                    Some("1.1"),
                    Some("hidden historical child"),
                    SpineTreeNodeStatus::Closed,
                ),
                root_epoch("2", Some("root"), SpineTreeNodeStatus::Opened),
                node(
                    "2.1",
                    Some("2"),
                    Some("active task"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        ));

        let lines = cell.display_lines(80);
        insta::assert_snapshot!(render(&lines), @r###"
        • Spine Tree
          ├ ◌ 1 previous leaf
          └ ◉ active task
        "###);
        let history_noun = lines[1]
            .spans
            .iter()
            .find(|span| span.content == "leaf")
            .expect("history bucket noun");
        assert_eq!(history_noun.style.fg, Some(Color::Green));
        assert!(!history_noun.style.add_modifier.contains(Modifier::BOLD));
        assert!(!history_noun.style.add_modifier.contains(Modifier::DIM));
        let history_previous = lines[1]
            .spans
            .iter()
            .find(|span| span.content == " previous ")
            .expect("history bucket previous label");
        assert_eq!(history_previous.style.fg, Some(Color::Green));
        assert!(!history_previous.style.add_modifier.contains(Modifier::BOLD));
        assert!(!history_previous.style.add_modifier.contains(Modifier::DIM));
        let history_count = lines[1]
            .spans
            .iter()
            .find(|span| span.content == "1")
            .expect("history bucket count");
        assert_eq!(history_count.style.fg, Some(Color::Green));
        assert!(!history_count.style.add_modifier.contains(Modifier::BOLD));
        assert!(!history_count.style.add_modifier.contains(Modifier::DIM));
        let raw = render(&cell.raw_lines());
        assert!(raw.contains("1 previous leaf"), "{raw}");
        assert!(!raw.contains("hidden historical child"));
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
    fn live_tail_keeps_tree_static_while_spawn_overlay_animates() {
        let mut state = SpineTreeViewState::new(true);
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
        assert!(live.transcript_animation_tick().is_some());
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

        assert_eq!(marker.style.fg, Some(Color::Cyan));
        assert!(marker.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(summary.style, Style::default());

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
    fn initial_snapshot_alone_does_not_create_a_live_tail() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("active"), SpineTreeNodeStatus::Live)],
        ));

        assert!(state.render_cell().is_none());
        assert!(state.snapshot_cell().is_some());
        assert!(state.take_pending_history_cell().is_none());
    }

    #[test]
    fn display_tree_change_does_not_create_a_live_tail_without_an_overlay() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
        ));

        let mut changed = snapshot(
            "1.1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Opened),
                node(
                    "1.1",
                    Some("1"),
                    Some("nested task"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        );
        changed.snapshot_seq = 2;
        state.apply_tree_update(changed);

        assert!(
            state.render_cell().is_none(),
            "an ordinary tree edge belongs in history, not the bottom live tail"
        );
        let history = state
            .take_pending_history_cell()
            .expect("the ordinary edge should emit one history effect");
        assert!(history.is_automatic_history());
        assert!(
            render(&history.display_lines(80)).contains("nested task"),
            "history must capture the accepted semantic edge"
        );
        assert!(
            state.take_pending_history_cell().is_none(),
            "the edge effect must be consumed exactly once"
        );
    }

    #[test]
    fn automatic_history_uses_the_same_renderer_as_an_explicit_snapshot() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
        ));
        let mut changed = snapshot(
            "1.1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Opened),
                node(
                    "1.1",
                    Some("1"),
                    Some("renderer equivalence"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        );
        changed.snapshot_seq = 2;
        state.apply_tree_update(changed);

        let automatic = state
            .take_pending_history_cell()
            .expect("automatic history");
        let explicit = state.snapshot_cell().expect("explicit snapshot");
        for width in [20, 80] {
            assert_eq!(
                automatic.display_lines(width),
                explicit.display_lines(width)
            );
        }
        assert_eq!(automatic.raw_lines(), explicit.raw_lines());
        assert!(automatic.is_automatic_history());
        assert!(!explicit.is_automatic_history());
    }

    #[test]
    fn projection_only_updates_do_not_create_a_live_tail() {
        let mut state = SpineTreeViewState::default();
        let initial = snapshot(
            "1",
            vec![node("1", None, Some("active"), SpineTreeNodeStatus::Live)],
        );
        state.apply_tree_update(initial.clone());

        let mut projection_only = initial;
        projection_only.turn_id = "later-turn".to_string();
        projection_only.snapshot_seq = 2;
        projection_only.settled_spawn_call_ids = vec!["settled-call".to_string()];
        projection_only.nodes[0].context_pressure =
            Some(codex_app_server_protocol::SpineNodeContextPressure {
                open_input_tokens: Some(100),
                current_input_tokens: Some(200),
                context_tokens: Some(300),
                problem: None,
            });
        state.apply_tree_update(projection_only);

        assert!(state.render_cell().is_none());
        assert!(state.take_pending_history_cell().is_none());
    }

    #[test]
    fn projection_only_updates_do_not_restore_a_consumed_tree_edge() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
        ));

        let mut changed = snapshot(
            "1.1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Opened),
                node(
                    "1.1",
                    Some("1"),
                    Some("nested task"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        );
        changed.snapshot_seq = 2;
        state.apply_tree_update(changed.clone());

        changed.turn_id = "status-followup".to_string();
        changed.snapshot_seq = 3;
        changed.nodes[1].context_pressure =
            Some(codex_app_server_protocol::SpineNodeContextPressure {
                open_input_tokens: Some(100),
                current_input_tokens: Some(200),
                context_tokens: Some(300),
                problem: None,
            });
        state.apply_tree_update(changed);

        assert!(
            state.render_cell().is_none(),
            "a projection-only update must not restore bottom live ownership"
        );
        let history = state
            .take_pending_history_cell()
            .expect("the prior semantic edge should remain pending once");
        assert_eq!(
            history.snapshot_seq(),
            2,
            "projection-only updates must not rewrite the edge-time presentation"
        );
        assert_eq!(history.turn_id(), "turn");
        assert!(render(&history.display_lines(80)).contains("nested task"));
        assert!(state.take_pending_history_cell().is_none());
    }

    #[test]
    fn stale_tree_change_does_not_create_a_live_tail() {
        let mut state = SpineTreeViewState::default();
        let mut current = snapshot(
            "1",
            vec![node("1", None, Some("current"), SpineTreeNodeStatus::Live)],
        );
        current.snapshot_seq = 2;
        state.apply_tree_update(current);

        let stale = snapshot(
            "1.1",
            vec![
                node("1", None, Some("current"), SpineTreeNodeStatus::Opened),
                node(
                    "1.1",
                    Some("1"),
                    Some("stale task"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        );
        state.apply_tree_update(stale);

        assert!(state.render_cell().is_none());
        assert_eq!(
            state
                .snapshot()
                .map(|snapshot| snapshot.active_node_id.as_str()),
            Some("1")
        );
        assert!(state.take_pending_history_cell().is_none());
    }

    #[test]
    fn spawn_outcome_change_creates_history_without_a_live_tail() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node(
                "1",
                None,
                Some("spawn result"),
                SpineTreeNodeStatus::Closed,
            )],
        ));

        let mut changed = snapshot(
            "1",
            vec![node(
                "1",
                None,
                Some("spawn result"),
                SpineTreeNodeStatus::Closed,
            )],
        );
        changed.snapshot_seq = 2;
        changed.nodes[0].spawn_outcome = Some(SpineSpawnOutcome::Completed);
        state.apply_tree_update(changed);

        assert!(state.render_cell().is_none());
        assert!(state.take_pending_history_cell().is_some());
    }

    #[test]
    fn settled_spawn_without_animation_commits_history() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
        ));
        state.apply_spawn_progress(SpineSpawnProgressUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "spawn-1".to_string(),
            tasks: vec![codex_app_server_protocol::SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "worker".to_string(),
                thread_id: "child-1".to_string(),
                agent_path: Some("/root/worker".to_string()),
                status: codex_app_server_protocol::CollabAgentStatus::Completed,
            }],
        });

        let mut committed = snapshot(
            "1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Live),
                node(
                    "1.1",
                    Some("1"),
                    Some("worker"),
                    SpineTreeNodeStatus::Closed,
                ),
            ],
        );
        committed.snapshot_seq = 2;
        committed.settled_spawn_call_ids = vec!["spawn-1".to_string()];
        state.apply_tree_update(committed);

        assert!(!state.has_spawn_call("spawn-1"));
        assert!(state.render_cell().is_none());
        let rendered = render(
            &state
                .take_pending_history_cell()
                .expect("settlement should commit visible history")
                .display_lines(80),
        );
        assert!(rendered.contains("worker"), "{rendered}");
    }

    #[test]
    fn settled_spawn_handoff_installs_authority_before_pretty_reveal() {
        let start = Instant::now();
        let mut state = SpineTreeViewState::new(true);
        state.apply_tree_update_at(
            snapshot(
                "1",
                vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
            ),
            start,
        );
        state.apply_spawn_progress(spawn_progress(
            "spawn-animated",
            &[
                (
                    "child-animated",
                    "retiring worker",
                    codex_app_server_protocol::CollabAgentStatus::Completed,
                ),
                (
                    "child-errored",
                    "errored worker",
                    codex_app_server_protocol::CollabAgentStatus::Errored,
                ),
                (
                    "child-aborted",
                    "aborted worker",
                    codex_app_server_protocol::CollabAgentStatus::Shutdown,
                ),
            ],
        ));

        let settle_now = Instant::now();
        let mut committed = snapshot(
            "1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Live),
                node(
                    "1.1",
                    Some("1"),
                    Some("imported worker"),
                    SpineTreeNodeStatus::Closed,
                ),
                node(
                    "1.2",
                    Some("1"),
                    Some("imported error"),
                    SpineTreeNodeStatus::Closed,
                ),
                node(
                    "1.3",
                    Some("1"),
                    Some("imported abort"),
                    SpineTreeNodeStatus::Closed,
                ),
            ],
        );
        committed.nodes[1].spawn_outcome = Some(SpineSpawnOutcome::Completed);
        committed.nodes[2].spawn_outcome = Some(SpineSpawnOutcome::Errored);
        committed.nodes[3].spawn_outcome = Some(SpineSpawnOutcome::Aborted);
        committed.snapshot_seq = 2;
        committed.settled_spawn_call_ids = vec!["spawn-animated".to_string()];
        state.apply_tree_update_at(committed.clone(), settle_now);

        assert_eq!(state.snapshot(), Some(&committed));
        let pending = state
            .pending_handoff
            .clone()
            .expect("matching settlement should retain the presentation");
        assert_eq!(pending.snapshot.snapshot_seq, 1);

        let cell = state.render_cell().expect("live presentation");
        assert!(cell.next_frame_in(settle_now).is_some());
        let active_pretty = render(&cell.display_lines_at(80, settle_now));
        assert!(active_pretty.contains("retiring worker"), "{active_pretty}");
        assert!(active_pretty.contains("errored worker"), "{active_pretty}");
        assert!(active_pretty.contains("aborted worker"), "{active_pretty}");
        assert!(
            !active_pretty.contains("imported worker"),
            "{active_pretty}"
        );
        let raw = render(&cell.raw_lines());
        assert!(raw.contains("imported worker"), "{raw}");
        assert!(raw.contains("imported error"), "{raw}");
        assert!(raw.contains("imported abort"), "{raw}");
        assert!(!raw.contains("retiring worker"), "{raw}");

        let revealed = render(&cell.display_lines_at(80, pending.reveal_at));
        assert_eq!(cell.next_frame_in(pending.reveal_at), None);
        assert!(!revealed.contains("retiring worker"), "{revealed}");
        assert!(revealed.contains("imported worker"), "{revealed}");
        let promoted = state
            .take_due_handoff_history(pending.reveal_at)
            .expect("due handoff should promote the final tree once");
        assert!(!state.has_spawn_call("spawn-animated"));
        assert!(state.pending_handoff.is_none());
        assert!(state.render_cell().is_none());
        assert!(render(&promoted.display_lines(80)).contains("imported worker"));
        assert!(state.take_due_handoff_history(pending.reveal_at).is_none());
    }

    #[test]
    fn clearing_turn_during_handoff_reveals_authoritative_tree() {
        let start = Instant::now();
        let mut state = SpineTreeViewState::new(true);
        state.apply_tree_update_at(
            snapshot(
                "1",
                vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
            ),
            start,
        );
        state.apply_spawn_progress(spawn_progress(
            "spawn-clear",
            &[(
                "child-clear",
                "retiring worker",
                codex_app_server_protocol::CollabAgentStatus::Completed,
            )],
        ));
        let mut committed = snapshot(
            "1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Live),
                node(
                    "1.1",
                    Some("1"),
                    Some("imported worker"),
                    SpineTreeNodeStatus::Closed,
                ),
            ],
        );
        committed.nodes[1].spawn_outcome = Some(SpineSpawnOutcome::Completed);
        committed.snapshot_seq = 2;
        committed.settled_spawn_call_ids = vec!["spawn-clear".to_string()];
        state.apply_tree_update_at(committed, start);
        assert!(state.pending_handoff.is_some());

        assert!(state.clear_incomplete_spawn_overlays(Some("turn")));
        assert!(state.pending_handoff.is_none());
        assert!(state.render_cell().is_none());
        let rendered = render(
            &state
                .take_pending_history_cell()
                .expect("clearing a handoff should retain the authoritative tree")
                .display_lines_at(80, start),
        );
        assert!(rendered.contains("imported worker"), "{rendered}");
        assert!(!rendered.contains("retiring worker"), "{rendered}");
    }

    #[test]
    fn handoff_mismatch_and_disabled_motion_reveal_immediately() {
        for animations_enabled in [true, false] {
            let start = Instant::now();
            let mut state = SpineTreeViewState::new(animations_enabled);
            state.apply_tree_update_at(
                snapshot(
                    "1",
                    vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
                ),
                start,
            );
            state.apply_spawn_progress(spawn_progress(
                "spawn-reveal",
                &[(
                    "child-reveal",
                    "retiring worker",
                    codex_app_server_protocol::CollabAgentStatus::Completed,
                )],
            ));

            let mut committed = snapshot(
                "1",
                vec![
                    node("1", None, Some("root"), SpineTreeNodeStatus::Live),
                    node(
                        "1.1",
                        Some("1"),
                        Some("imported worker"),
                        SpineTreeNodeStatus::Closed,
                    ),
                ],
            );
            committed.nodes[1].spawn_outcome = Some(if animations_enabled {
                SpineSpawnOutcome::Errored
            } else {
                SpineSpawnOutcome::Completed
            });
            committed.snapshot_seq = 2;
            committed.settled_spawn_call_ids = vec!["spawn-reveal".to_string()];
            state.apply_tree_update_at(committed, Instant::now());

            assert!(state.pending_handoff.is_none());
            assert!(!state.has_spawn_call("spawn-reveal"));
            assert!(state.render_cell().is_none());
            let rendered = render(
                &state
                    .take_pending_history_cell()
                    .expect("non-animated settlement should commit history")
                    .display_lines(80),
            );
            assert!(rendered.contains("imported worker"), "{rendered}");
            assert!(!rendered.contains("retiring worker"), "{rendered}");
        }
    }

    #[test]
    fn missing_settled_overlay_supersedes_handoff_into_history() {
        let mut state = SpineTreeViewState::new(true);
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
        ));
        state.apply_spawn_progress(spawn_progress(
            "spawn-present",
            &[(
                "child-present",
                "retiring worker",
                codex_app_server_protocol::CollabAgentStatus::Completed,
            )],
        ));
        let mut committed = snapshot(
            "1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Live),
                node(
                    "1.1",
                    Some("1"),
                    Some("imported worker"),
                    SpineTreeNodeStatus::Closed,
                ),
            ],
        );
        committed.nodes[1].spawn_outcome = Some(SpineSpawnOutcome::Completed);
        committed.snapshot_seq = 2;
        committed.settled_spawn_call_ids = vec!["spawn-present".to_string()];
        state.apply_tree_update(committed.clone());
        assert!(state.pending_handoff.is_some());

        committed.snapshot_seq = 3;
        committed
            .settled_spawn_call_ids
            .push("spawn-missing".to_string());
        state.apply_tree_update(committed);

        assert!(state.pending_handoff.is_none());
        assert!(!state.has_spawn_call("spawn-present"));
        assert!(state.render_cell().is_none());
        assert!(state.take_pending_history_cell().is_some());
    }

    #[test]
    fn settled_call_order_controls_handoff() {
        for (settled_calls, expect_handoff) in [
            (["spawn-completed", "spawn-errored"], true),
            (["spawn-errored", "spawn-completed"], false),
        ] {
            let mut state = SpineTreeViewState::new(true);
            state.apply_tree_update(snapshot(
                "1",
                vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
            ));
            state.apply_spawn_progress(spawn_progress(
                "spawn-completed",
                &[(
                    "child-completed",
                    "completed worker",
                    codex_app_server_protocol::CollabAgentStatus::Completed,
                )],
            ));
            state.apply_spawn_progress(spawn_progress(
                "spawn-errored",
                &[(
                    "child-errored",
                    "errored worker",
                    codex_app_server_protocol::CollabAgentStatus::Errored,
                )],
            ));
            let mut committed = snapshot(
                "1",
                vec![
                    node("1", None, Some("root"), SpineTreeNodeStatus::Live),
                    node(
                        "1.1",
                        Some("1"),
                        Some("imported completion"),
                        SpineTreeNodeStatus::Closed,
                    ),
                    node(
                        "1.2",
                        Some("1"),
                        Some("imported error"),
                        SpineTreeNodeStatus::Closed,
                    ),
                ],
            );
            committed.nodes[1].spawn_outcome = Some(SpineSpawnOutcome::Completed);
            committed.nodes[2].spawn_outcome = Some(SpineSpawnOutcome::Errored);
            committed.snapshot_seq = 2;
            committed.settled_spawn_call_ids =
                settled_calls.map(str::to_string).into_iter().collect();
            state.apply_tree_update(committed);

            assert_eq!(state.pending_handoff.is_some(), expect_handoff);
            assert!(!state.has_spawn_call("spawn-completed"));
            assert!(!state.has_spawn_call("spawn-errored"));
        }
    }

    #[test]
    fn changed_authoritative_outcome_finishes_an_active_handoff_fail_open() {
        let start = Instant::now();
        let mut state = SpineTreeViewState::new(true);
        state.apply_tree_update_at(
            snapshot(
                "1",
                vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
            ),
            start,
        );
        state.apply_spawn_progress(spawn_progress(
            "spawn-generation",
            &[(
                "child-generation",
                "retiring worker",
                codex_app_server_protocol::CollabAgentStatus::Completed,
            )],
        ));
        let mut committed = snapshot(
            "1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Live),
                node(
                    "1.1",
                    Some("1"),
                    Some("imported worker"),
                    SpineTreeNodeStatus::Closed,
                ),
            ],
        );
        committed.nodes[1].spawn_outcome = Some(SpineSpawnOutcome::Completed);
        committed.snapshot_seq = 2;
        committed.settled_spawn_call_ids = vec!["spawn-generation".to_string()];
        state.apply_tree_update_at(committed.clone(), Instant::now());
        assert!(state.pending_handoff.is_some());

        committed.snapshot_seq = 3;
        committed.nodes[1].spawn_outcome = Some(SpineSpawnOutcome::Errored);
        state.apply_tree_update_at(committed, Instant::now());

        assert!(state.pending_handoff.is_none());
        assert!(!state.has_spawn_call("spawn-generation"));
        assert!(state.render_cell().is_none());
        let rendered = render(
            &state
                .take_pending_history_cell()
                .expect("superseding authority should commit history")
                .display_lines(80),
        );
        assert!(rendered.contains("imported worker"), "{rendered}");
    }

    #[test]
    fn inactive_tree_edges_coalesce_to_the_latest_history_effect() {
        let mut state = SpineTreeViewState::default();
        state.apply_tree_update(snapshot(
            "1",
            vec![node("1", None, Some("root"), SpineTreeNodeStatus::Live)],
        ));

        let mut first = snapshot(
            "1.1",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Opened),
                node(
                    "1.1",
                    Some("1"),
                    Some("first inactive edge"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        );
        first.snapshot_seq = 2;
        state.apply_tree_update(first);

        let mut latest = snapshot(
            "1.2",
            vec![
                node("1", None, Some("root"), SpineTreeNodeStatus::Opened),
                node(
                    "1.2",
                    Some("1"),
                    Some("latest inactive edge"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        );
        latest.snapshot_seq = 3;
        state.apply_tree_update(latest);

        let history = state
            .take_pending_history_cell()
            .expect("inactive edges should retain one pending presentation");
        let rendered = render(&history.display_lines(80));
        assert!(!rendered.contains("first inactive edge"), "{rendered}");
        assert!(rendered.contains("latest inactive edge"), "{rendered}");
        assert_eq!(history.snapshot_seq(), 3);
        assert!(state.take_pending_history_cell().is_none());
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
        assert!(!task_line.contains('•'), "{rendered}");
        assert!(!task_line.contains('◦'), "{rendered}");
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
