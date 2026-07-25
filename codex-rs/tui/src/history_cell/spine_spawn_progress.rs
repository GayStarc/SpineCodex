use crate::motion::MotionMode;
use crate::motion::ORGANIC_ACTIVITY_WORDS;
use crate::motion::ReducedMotionIndicator;
use crate::motion::green_activity_indicator;
use crate::motion::green_shimmer_text;
use crate::multi_agents::AgentActivityTracker;
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
use std::time::Instant;

const ACTIVITY_PREVIEW_LINES: usize = 4;
const ACTIVITY_INDENT: &str = "   ";

#[derive(Debug, Clone)]
pub(crate) struct SpineSpawnOverlay {
    notification: SpineSpawnProgressUpdatedNotification,
    activity: HashMap<String, AgentActivityTracker>,
    activity_words: HashMap<String, String>,
    started_at: Instant,
}

impl SpineSpawnOverlay {
    pub(crate) fn new(notification: SpineSpawnProgressUpdatedNotification) -> Self {
        let activity_words = random_activity_words(&notification.tasks, &HashMap::new());
        Self {
            notification,
            activity: HashMap::new(),
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
        apply_notification(task, tracker, notification, status)
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
                self.started_at,
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
            let preview = self
                .activity
                .get(task.thread_id.as_str())
                .map(AgentActivityTracker::preview);
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

    #[cfg(test)]
    pub(crate) fn activity_word(&self, thread_id: &str) -> Option<&str> {
        self.activity_words.get(thread_id).map(String::as_str)
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
    started_at: Instant,
    animations_enabled: bool,
) -> Vec<Span<'static>> {
    if *status == CollabAgentStatus::Running {
        let motion_mode = MotionMode::from_animations_enabled(animations_enabled);
        let mut spans = Vec::new();
        if let Some(indicator) = green_activity_indicator(
            Some(started_at),
            motion_mode,
            ReducedMotionIndicator::StaticBullet,
        ) {
            spans.push(indicator);
            spans.push(" ".into());
        }
        spans.extend(green_shimmer_text(activity_word, motion_mode));
        return spans;
    }

    vec![
        status_span(status),
        " ".into(),
        Span::from(activity_word.to_string()).green(),
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
