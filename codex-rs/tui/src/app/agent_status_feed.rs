//! Bounded, best-effort subagent activity previews.
//!
//! The native `/subagents` picker uses these previews for its selected row. The legacy history
//! cell remains test-only so production navigation has a single interactive presentation.

use super::ThreadEventStore;
#[cfg(test)]
use crate::history_cell::HistoryCell;
#[cfg(test)]
use crate::history_cell::plain_lines;
use crate::multi_agents::AgentActivityPathDisplay;
use crate::multi_agents::AgentActivityPreview;
#[cfg(test)]
use ratatui::style::Stylize;
use ratatui::text::Line;

#[cfg(test)]
const AGENT_STATUS_PREVIEW_INDENT: u16 = 4;

#[cfg(test)]
#[derive(Debug)]
pub(super) struct AgentStatusHistoryCell {
    entries: Vec<AgentStatusThreadPreview>,
}

#[cfg(test)]
impl AgentStatusHistoryCell {
    pub(super) fn new(entries: Vec<AgentStatusThreadPreview>) -> Self {
        Self { entries }
    }
}

#[cfg(test)]
impl HistoryCell for AgentStatusHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = vec![
            "/agent".magenta().into(),
            "Sub-agents running".bold().into(),
            "".into(),
        ];

        if self.entries.is_empty() {
            lines.push("  • No sub-agents running.".italic().into());
            return lines;
        }

        for entry in &self.entries {
            lines.push(entry.title_line());
            let preview_width = width.saturating_sub(AGENT_STATUS_PREVIEW_INDENT).max(1);
            let preview_lines = entry.preview_lines(preview_width);
            if preview_lines.is_empty() {
                lines.push(vec!["    ".into(), "No recent activity yet.".dim().italic()].into());
            } else {
                lines.extend(preview_lines.into_iter().map(indent_preview_line));
            }
            lines.push("".into());
        }
        let _ = lines.pop();
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(u16::MAX))
    }
}

#[derive(Debug)]
pub(super) struct AgentStatusThreadPreview {
    #[cfg(test)]
    agent_path: String,
    activity: AgentActivityPreview,
}

impl AgentStatusThreadPreview {
    pub(super) fn from_store(_agent_path: String, store: &ThreadEventStore) -> Self {
        Self {
            #[cfg(test)]
            agent_path: _agent_path,
            activity: store.agent_activity_preview(AgentActivityPathDisplay::Show),
        }
    }

    #[cfg(test)]
    fn title_line(&self) -> Line<'static> {
        vec!["  • ".dim(), format!("`{}`", self.agent_path).cyan()].into()
    }

    fn preview_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.activity.lines(width)
    }

    pub(super) fn activity_summary(&self, width: u16) -> String {
        self.preview_lines(width)
            .first()
            .map(ToString::to_string)
            .unwrap_or_else(|| "No recent activity yet.".to_string())
    }
}

#[cfg(test)]
fn indent_preview_line(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, "    ".into());
    line
}

#[cfg(test)]
#[path = "agent_status_feed_tests.rs"]
mod tests;
