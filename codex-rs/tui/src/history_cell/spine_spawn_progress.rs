use crate::motion::MotionMode;
use crate::motion::ORGANIC_ACTIVITY_WORDS;
use crate::motion::spine_brand_shimmer_text;
use crate::multi_agents::AgentActivityPreview;
use crate::multi_agents::AgentActivityTracker;
use crate::product_brand::SPINE_BRAND_COLOR;
use crate::render::line_utils::push_owned_lines;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_line;
use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SpineSpawnProgressUpdatedNotification;
use codex_app_server_protocol::SpineSpawnTaskProgress;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnStatus;
use rand::Rng;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const ACTIVITY_PREVIEW_LINES: usize = 4;
const ACTIVITY_INDENT: &str = "   ";
const ACTIVITY_REVEAL_COLUMNS_PER_SECOND: f64 = 40.0;
const ACTIVITY_REVEAL_MAX_PENDING_COLUMNS: usize = 160;
const ACTIVITY_REVEAL_MAX_VISIBLE_COLUMNS: usize = 480;

#[derive(Debug, Clone)]
pub(crate) struct SpineSpawnOverlay {
    notification: SpineSpawnProgressUpdatedNotification,
    activity: HashMap<String, AgentActivityTracker>,
    activity_reveal: HashMap<String, FlowingActivityText>,
    activity_words: HashMap<String, String>,
    started_at: Instant,
}

impl SpineSpawnOverlay {
    pub(crate) fn new(notification: SpineSpawnProgressUpdatedNotification) -> Self {
        let activity_words = random_activity_words(&notification.tasks, &HashMap::new());
        Self {
            notification,
            activity: HashMap::new(),
            activity_reveal: HashMap::new(),
            activity_words,
            started_at: Instant::now(),
        }
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.notification.call_id
    }

    pub(crate) fn turn_id(&self) -> &str {
        &self.notification.turn_id
    }

    pub(crate) fn replace_notification(
        &mut self,
        mut notification: SpineSpawnProgressUpdatedNotification,
    ) {
        for task in &mut notification.tasks {
            if let Some(current) = self
                .notification
                .tasks
                .iter()
                .find(|current| current.thread_id == task.thread_id)
            {
                task.status = merged_status(&current.status, task.status.clone());
            }
        }
        self.notification = notification;
        self.activity.retain(|thread_id, _| {
            self.notification
                .tasks
                .iter()
                .any(|task| task.thread_id == *thread_id)
        });
        self.activity_reveal.retain(|thread_id, _| {
            self.notification
                .tasks
                .iter()
                .any(|task| task.thread_id == *thread_id)
        });
        self.activity_words = random_activity_words(&self.notification.tasks, &self.activity_words);
    }

    pub(crate) fn seed_activity(
        &mut self,
        thread_id: &str,
        notifications: impl Iterator<Item = ServerNotification>,
    ) -> bool {
        let Some(task_index) = self
            .notification
            .tasks
            .iter()
            .position(|task| task.thread_id == thread_id)
        else {
            return false;
        };
        let changed = {
            let tracker = self.activity.entry(thread_id.to_string()).or_default();
            let mut changed = false;
            for notification in notifications {
                changed |= apply_notification(
                    &mut self.notification.tasks[task_index],
                    tracker,
                    &notification,
                    spine_spawn_status(&notification),
                );
            }
            changed
        };
        if changed {
            self.retarget_activity(thread_id, Instant::now());
        }
        changed
    }

    pub(crate) fn update_activity(
        &mut self,
        thread_id: &str,
        notification: &ServerNotification,
        status: Option<CollabAgentStatus>,
    ) -> bool {
        let Some(task) = self
            .notification
            .tasks
            .iter_mut()
            .find(|task| task.thread_id == thread_id)
        else {
            return false;
        };
        let tracker = self.activity.entry(thread_id.to_string()).or_default();
        let changed = apply_notification(task, tracker, notification, status);
        if changed {
            self.retarget_activity(thread_id, Instant::now());
        }
        changed
    }

    pub(crate) fn has_child_thread(&self, thread_id: &str) -> bool {
        self.notification
            .tasks
            .iter()
            .any(|task| task.thread_id == thread_id)
    }

    pub(crate) fn has_activity(&self, thread_id: &str) -> bool {
        self.activity.contains_key(thread_id)
    }

    pub(crate) fn update_status(&mut self, thread_id: &str, status: CollabAgentStatus) -> bool {
        let Some(task) = self
            .notification
            .tasks
            .iter_mut()
            .find(|task| task.thread_id == thread_id)
        else {
            return false;
        };
        apply_status(&mut task.status, status)
    }

    pub(crate) fn display_lines(
        &self,
        prefix: &str,
        is_last: bool,
        width: u16,
        animations_enabled: bool,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let task_prefix = prefix.to_string();
        let task_count = self.notification.tasks.len();
        for (index, task) in self.notification.tasks.iter().enumerate() {
            let task_is_last = is_last && index + 1 == task_count;
            let activity_word = self
                .activity_words
                .get(task.thread_id.as_str())
                .map(String::as_str)
                .unwrap_or("Branching");
            let mut label_spans =
                vec![Span::from(format!("{task_prefix}{}", branch(task_is_last))).dim()];
            label_spans.extend(status_and_activity_word_spans(
                &task.status,
                activity_word,
                animations_enabled,
            ));
            label_spans.push(" ".into());
            label_spans.push(task.summary.trim().to_string().into());
            let label_line = Line::from(label_spans);
            let continuation = Span::from(format!("{task_prefix}{}  ", child_prefix(task_is_last)))
                .dim()
                .into();
            push_wrapped_line(label_line, continuation, width, &mut lines);

            if !matches!(
                task.status,
                CollabAgentStatus::PendingInit | CollabAgentStatus::Running
            ) {
                continue;
            }
            let activity_prefix = format!(
                "{task_prefix}{}{ACTIVITY_INDENT}",
                child_prefix(task_is_last)
            );
            let activity_width = width
                .saturating_sub(activity_prefix.chars().count() as u16)
                .max(1);
            let preview = self.activity.get(task.thread_id.as_str()).map(|tracker| {
                if animations_enabled {
                    self.activity_reveal
                        .get(task.thread_id.as_str())
                        .map(|reveal| {
                            AgentActivityPreview::from_flow_text(
                                &reveal.visible_text_at(Instant::now()),
                            )
                        })
                        .unwrap_or_default()
                } else {
                    tracker.preview()
                }
            });
            let mut preview_lines = preview
                .as_ref()
                .map(|preview| preview.lines_with_limit(activity_width, ACTIVITY_PREVIEW_LINES))
                .unwrap_or_default();
            if preview_lines.is_empty() {
                let empty_state = match task.status {
                    CollabAgentStatus::PendingInit => "Waiting to start...",
                    CollabAgentStatus::Running => "Waiting for activity...",
                    _ => unreachable!("activity preview only renders pending/running tasks"),
                };
                preview_lines.push(empty_state.dim().italic().into());
            }
            while preview_lines.len() < ACTIVITY_PREVIEW_LINES {
                preview_lines.push(Line::default());
            }
            lines.extend(
                preview_lines
                    .into_iter()
                    .take(ACTIVITY_PREVIEW_LINES)
                    .map(|mut line| {
                        line.spans
                            .insert(0, Span::from(activity_prefix.clone()).dim());
                        line
                    }),
            );
            lines.push(activity_separator(&task_prefix, task_is_last));
        }
        lines
    }

    pub(crate) fn animation_start(&self) -> Instant {
        self.started_at
    }

    fn retarget_activity(&mut self, thread_id: &str, now: Instant) {
        let Some(target) = self
            .activity
            .get(thread_id)
            .map(AgentActivityTracker::preview)
            .map(|preview| preview.flow_text())
        else {
            return;
        };
        match self.activity_reveal.get_mut(thread_id) {
            Some(reveal) => reveal.retarget(target, now),
            None => {
                self.activity_reveal
                    .insert(thread_id.to_string(), FlowingActivityText::new(target, now));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn activity_word(&self, thread_id: &str) -> Option<&str> {
        self.activity_words.get(thread_id).map(String::as_str)
    }
}

#[derive(Debug, Clone)]
struct FlowingActivityText {
    source: String,
    visible: String,
    pending: String,
    revealed_columns_at_retarget: f64,
    retargeted_at: Instant,
}

impl FlowingActivityText {
    fn new(source: String, now: Instant) -> Self {
        Self {
            pending: trailing_display_columns(&source, ACTIVITY_REVEAL_MAX_PENDING_COLUMNS),
            source,
            visible: String::new(),
            revealed_columns_at_retarget: 0.0,
            retargeted_at: now,
        }
    }

    fn retarget(&mut self, source: String, now: Instant) {
        if self.source == source {
            return;
        }

        let reveal_budget = self.revealed_columns(now);
        let (newly_visible, remaining, consumed_columns) =
            split_at_display_columns(&self.pending, reveal_budget.floor() as usize);
        self.visible.push_str(newly_visible);
        self.visible = trailing_display_columns(&self.visible, ACTIVITY_REVEAL_MAX_VISIBLE_COLUMNS);

        let source_appended = source.starts_with(&self.source);
        let source_continues =
            source_appended || suffix_prefix_overlap_bytes(&self.source, &source) > 0;
        let mut pending = if source_appended {
            let mut pending = remaining.to_string();
            pending.push_str(&source[self.source.len()..]);
            pending
        } else {
            let overlap = suffix_prefix_overlap_bytes(&self.visible, &source);
            if overlap > 0 {
                source[overlap..].to_string()
            } else {
                if !self.visible.is_empty() && !self.visible.ends_with('\n') {
                    self.visible.push('\n');
                }
                source.clone()
            }
        };

        let pending_width = UnicodeWidthStr::width(pending.as_str());
        let pending_was_trimmed = pending_width > ACTIVITY_REVEAL_MAX_PENDING_COLUMNS;
        if pending_was_trimmed {
            pending = trailing_display_columns(&pending, ACTIVITY_REVEAL_MAX_PENDING_COLUMNS);
            if !self.visible.is_empty() && !self.visible.ends_with('\n') {
                self.visible.push('\n');
            }
        }
        self.visible = trailing_display_columns(&self.visible, ACTIVITY_REVEAL_MAX_VISIBLE_COLUMNS);

        self.source = source;
        self.pending = pending;
        self.revealed_columns_at_retarget = if source_continues {
            (reveal_budget - consumed_columns as f64).max(0.0)
        } else {
            0.0
        };
        self.retargeted_at = now;
    }

    fn visible_text_at(&self, now: Instant) -> String {
        let (revealed, _, _) =
            split_at_display_columns(&self.pending, self.revealed_columns(now).floor() as usize);
        let mut visible = String::with_capacity(self.visible.len() + revealed.len());
        visible.push_str(&self.visible);
        visible.push_str(revealed);
        visible
    }

    fn revealed_columns(&self, now: Instant) -> f64 {
        let elapsed = now
            .checked_duration_since(self.retargeted_at)
            .unwrap_or(Duration::ZERO);
        (self.revealed_columns_at_retarget
            + elapsed.as_secs_f64() * ACTIVITY_REVEAL_COLUMNS_PER_SECOND)
            .min(UnicodeWidthStr::width(self.pending.as_str()) as f64)
    }
}

fn split_at_display_columns(text: &str, budget: usize) -> (&str, &str, usize) {
    let mut byte_offset = 0;
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let next = width + UnicodeWidthStr::width(grapheme);
        if next > budget {
            break;
        }
        width = next;
        byte_offset += grapheme.len();
    }
    (&text[..byte_offset], &text[byte_offset..], width)
}

fn trailing_display_columns(text: &str, max_columns: usize) -> String {
    let graphemes = text.grapheme_indices(true).collect::<Vec<_>>();
    let mut width = 0;
    let start = (0..graphemes.len())
        .rev()
        .take_while(|&index| {
            let grapheme = graphemes[index].1;
            let next = width + UnicodeWidthStr::width(grapheme);
            if next > max_columns {
                return false;
            }
            width = next;
            true
        })
        .last()
        .map(|index| graphemes[index].0)
        .unwrap_or(text.len());
    text[start..].to_string()
}

fn suffix_prefix_overlap_bytes(left: &str, right: &str) -> usize {
    let left_graphemes = left.grapheme_indices(true).collect::<Vec<_>>();
    let right_graphemes = right.graphemes(true).collect::<Vec<_>>();
    (1..=left_graphemes.len().min(right_graphemes.len()))
        .rev()
        .find(|&len| {
            left_graphemes[left_graphemes.len() - len..]
                .iter()
                .map(|(_, grapheme)| *grapheme)
                .eq(right_graphemes[..len].iter().copied())
        })
        .map(|len| left.len() - left_graphemes[left_graphemes.len() - len].0)
        .unwrap_or(0)
}

#[cfg(test)]
mod flowing_activity_tests {
    use super::*;

    #[test]
    fn activity_reveal_advances_at_a_fixed_column_rate() {
        let start = Instant::now();
        let reveal = FlowingActivityText::new("abcdefghij".to_string(), start);
        assert_eq!(reveal.visible_text_at(start), "");
        assert_eq!(
            reveal.visible_text_at(start + Duration::from_millis(125)),
            "abcde"
        );
    }

    #[test]
    fn appended_and_sliding_targets_do_not_flush_new_text() {
        let start = Instant::now();
        let mut reveal = FlowingActivityText::new("abcdef".to_string(), start);
        let update = start + Duration::from_millis(100);
        reveal.retarget("abcdefghij".to_string(), update);
        assert_eq!(reveal.visible_text_at(update), "abcd");
        assert_eq!(
            reveal.visible_text_at(update + Duration::from_millis(100)),
            "abcdefgh"
        );

        reveal.retarget("defghijk".to_string(), update + Duration::from_millis(100));
        assert_eq!(
            reveal.visible_text_at(update + Duration::from_millis(100)),
            "abcdefgh"
        );
        assert_eq!(
            reveal.visible_text_at(update + Duration::from_millis(200)),
            "abcdefghijk"
        );
    }

    #[test]
    fn sustained_updates_bound_unrevealed_backlog_without_flushing() {
        let start = Instant::now();
        let mut reveal = FlowingActivityText::new("a".repeat(400), start);
        assert_eq!(
            UnicodeWidthStr::width(reveal.pending.as_str()),
            ACTIVITY_REVEAL_MAX_PENDING_COLUMNS
        );

        let update = start + Duration::from_secs(1);
        assert_eq!(
            UnicodeWidthStr::width(reveal.visible_text_at(update).as_str()),
            40
        );
        reveal.retarget(format!("{}{}", "a".repeat(400), "b".repeat(100)), update);

        assert_eq!(
            UnicodeWidthStr::width(reveal.pending.as_str()),
            ACTIVITY_REVEAL_MAX_PENDING_COLUMNS
        );
        let visible_at_update = reveal.visible_text_at(update).replace('\n', "");
        assert_eq!(UnicodeWidthStr::width(visible_at_update.as_str()), 40);
        let visible_after_tick = reveal
            .visible_text_at(update + Duration::from_millis(100))
            .replace('\n', "");
        assert_eq!(UnicodeWidthStr::width(visible_after_tick.as_str()), 44);
    }

    #[test]
    fn subframe_sliding_updates_preserve_fractional_reveal_budget() {
        let start = Instant::now();
        let mut source = "a".repeat(400);
        let mut reveal = FlowingActivityText::new(source.clone(), start);

        for step in 1..=10 {
            source.remove(0);
            source.push('b');
            reveal.retarget(source.clone(), start + Duration::from_millis(step * 10));
        }

        let visible = reveal
            .visible_text_at(start + Duration::from_millis(100))
            .replace('\n', "");
        assert_eq!(UnicodeWidthStr::width(visible.as_str()), 4);
        assert!(
            UnicodeWidthStr::width(reveal.visible.as_str()) <= ACTIVITY_REVEAL_MAX_VISIBLE_COLUMNS
        );
    }
}

fn apply_notification(
    task: &mut SpineSpawnTaskProgress,
    tracker: &mut AgentActivityTracker,
    notification: &ServerNotification,
    status: Option<CollabAgentStatus>,
) -> bool {
    let activity_changed = tracker.apply(notification);
    let status_changed = status.is_some_and(|status| apply_status(&mut task.status, status));
    let inferred_running = activity_changed
        && task.status == CollabAgentStatus::PendingInit
        && apply_status(&mut task.status, CollabAgentStatus::Running);
    activity_changed || status_changed || inferred_running
}

fn apply_status(current: &mut CollabAgentStatus, incoming: CollabAgentStatus) -> bool {
    let next = merged_status(current, incoming);
    if *current == next {
        return false;
    }
    *current = next;
    true
}

fn merged_status(current: &CollabAgentStatus, incoming: CollabAgentStatus) -> CollabAgentStatus {
    let current_is_terminal = matches!(
        current,
        CollabAgentStatus::Interrupted
            | CollabAgentStatus::Completed
            | CollabAgentStatus::Errored
            | CollabAgentStatus::Shutdown
            | CollabAgentStatus::NotFound
    );
    if (*current != CollabAgentStatus::PendingInit && incoming == CollabAgentStatus::PendingInit)
        || (current_is_terminal && incoming == CollabAgentStatus::Running)
    {
        return current.clone();
    }
    incoming
}

pub(crate) fn spine_spawn_status(notification: &ServerNotification) -> Option<CollabAgentStatus> {
    match notification {
        ServerNotification::TurnStarted(_) => Some(CollabAgentStatus::Running),
        ServerNotification::TurnCompleted(notification) => Some(match notification.turn.status {
            TurnStatus::Completed => CollabAgentStatus::Completed,
            TurnStatus::Interrupted => CollabAgentStatus::Interrupted,
            TurnStatus::Failed => CollabAgentStatus::Errored,
            TurnStatus::InProgress => CollabAgentStatus::Running,
        }),
        ServerNotification::ThreadStatusChanged(notification) => match notification.status {
            ThreadStatus::Active { .. } => Some(CollabAgentStatus::Running),
            ThreadStatus::SystemError => Some(CollabAgentStatus::Errored),
            ThreadStatus::NotLoaded | ThreadStatus::Idle => None,
        },
        _ => None,
    }
}

fn random_activity_words(
    tasks: &[SpineSpawnTaskProgress],
    existing: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut assigned = existing
        .iter()
        .filter(|(thread_id, _)| tasks.iter().any(|task| task.thread_id == **thread_id))
        .map(|(thread_id, word)| (thread_id.clone(), word.clone()))
        .collect::<HashMap<_, _>>();
    let mut available = ORGANIC_ACTIVITY_WORDS
        .iter()
        .copied()
        .filter(|word| !assigned.values().any(|assigned| assigned == word))
        .collect::<Vec<_>>();
    let mut rng = rand::rng();
    for task in tasks {
        if assigned.contains_key(&task.thread_id) {
            continue;
        }
        let word = if !available.is_empty() {
            let index = rng.random_range(0..available.len());
            available.swap_remove(index).to_string()
        } else {
            let base = ORGANIC_ACTIVITY_WORDS[rng.random_range(0..ORGANIC_ACTIVITY_WORDS.len())];
            let mut label = format!("Further {base}");
            while assigned.values().any(|assigned| assigned == &label) {
                label.insert_str(0, "Further ");
            }
            label
        };
        assigned.insert(task.thread_id.clone(), word);
    }
    assigned
}

fn status_and_activity_word_spans(
    status: &CollabAgentStatus,
    activity_word: &str,
    animations_enabled: bool,
) -> Vec<Span<'static>> {
    if *status == CollabAgentStatus::Running {
        let motion_mode = MotionMode::from_animations_enabled(animations_enabled);
        return spine_brand_shimmer_text(activity_word, motion_mode);
    }

    vec![
        status_span(status),
        " ".into(),
        Span::from(activity_word.to_string()).fg(SPINE_BRAND_COLOR),
    ]
}

fn activity_separator(prefix: &str, task_is_last: bool) -> Line<'static> {
    if task_is_last {
        Line::default()
    } else {
        Span::from(format!("{prefix}│")).dim().into()
    }
}

fn push_wrapped_line(
    line: Line<'static>,
    continuation: Line<'static>,
    width: u16,
    out: &mut Vec<Line<'static>>,
) {
    let wrapped = adaptive_wrap_line(
        &line,
        RtOptions::new(width.max(1) as usize).subsequent_indent(continuation),
    );
    push_owned_lines(&wrapped, out);
}

fn branch(is_last: bool) -> &'static str {
    if is_last { "└ " } else { "├ " }
}

fn child_prefix(is_last: bool) -> &'static str {
    if is_last { "  " } else { "│ " }
}

fn status_span(status: &CollabAgentStatus) -> Span<'static> {
    match status {
        CollabAgentStatus::PendingInit => "◌".cyan(),
        CollabAgentStatus::Running => "◐".cyan().bold(),
        CollabAgentStatus::Completed => "✓".green(),
        CollabAgentStatus::Interrupted => "!".yellow(),
        CollabAgentStatus::Errored | CollabAgentStatus::NotFound => "×".red(),
        CollabAgentStatus::Shutdown => "×".dim(),
    }
}
