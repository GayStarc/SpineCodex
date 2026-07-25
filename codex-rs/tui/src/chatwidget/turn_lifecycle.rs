//! Agent-turn lifecycle state for `ChatWidget`.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

use codex_utils_sleep_inhibitor::SleepInhibitor;

#[derive(Debug)]
pub(super) struct TurnLifecycleState {
    pub(super) sleep_inhibitor: SleepInhibitor,
    /// Tracks whether codex-core currently considers an agent turn to be in progress.
    pub(super) agent_turn_running: bool,
    pub(super) agent_turn_started_at: Option<Instant>,
    turn_presentation_alive: Option<Arc<AtomicBool>>,
    pub(super) last_turn_id: Option<String>,
    pub(super) budget_limited_turn_ids: HashSet<String>,
}

impl TurnLifecycleState {
    pub(super) fn new(prevent_idle_sleep: bool) -> Self {
        Self {
            sleep_inhibitor: SleepInhibitor::new(prevent_idle_sleep),
            agent_turn_running: false,
            agent_turn_started_at: None,
            turn_presentation_alive: None,
            last_turn_id: None,
            budget_limited_turn_ids: HashSet::new(),
        }
    }

    pub(super) fn start(&mut self, now: Instant) {
        self.end_turn_presentation();
        self.agent_turn_running = true;
        self.agent_turn_started_at = Some(now);
        self.turn_presentation_alive = Some(Arc::new(AtomicBool::new(true)));
        self.sleep_inhibitor.set_turn_running(/*turn_running*/ true);
    }

    pub(super) fn finish(&mut self) {
        self.end_turn_presentation();
        self.agent_turn_running = false;
        self.agent_turn_started_at = None;
        self.sleep_inhibitor
            .set_turn_running(/*turn_running*/ false);
    }

    pub(super) fn restore_running(&mut self, running: bool, now: Instant) {
        self.end_turn_presentation();
        self.agent_turn_running = running;
        self.agent_turn_started_at = running.then_some(now);
        self.turn_presentation_alive = running.then(|| Arc::new(AtomicBool::new(true)));
        self.sleep_inhibitor.set_turn_running(running);
    }

    pub(super) fn presentation_alive(&self) -> Option<Arc<AtomicBool>> {
        self.agent_turn_running
            .then(|| self.turn_presentation_alive.as_ref().cloned())
            .flatten()
    }

    pub(super) fn set_presentation_enabled(&mut self, enabled: bool) {
        if enabled && self.agent_turn_running && self.turn_presentation_alive.is_none() {
            self.turn_presentation_alive = Some(Arc::new(AtomicBool::new(true)));
        } else if !enabled {
            self.end_turn_presentation();
        }
    }

    pub(super) fn end_turn_presentation(&mut self) {
        if let Some(alive) = self.turn_presentation_alive.take() {
            alive.store(false, Ordering::Relaxed);
        }
    }

    pub(super) fn reset_thread(&mut self) {
        self.finish();
        self.last_turn_id = None;
        self.budget_limited_turn_ids.clear();
    }

    pub(super) fn set_prevent_idle_sleep(&mut self, enabled: bool) {
        self.sleep_inhibitor = SleepInhibitor::new(enabled);
        self.sleep_inhibitor
            .set_turn_running(self.agent_turn_running);
    }

    pub(super) fn mark_budget_limited(&mut self, turn_id: String) {
        self.budget_limited_turn_ids.insert(turn_id);
    }

    pub(super) fn take_budget_limited(&mut self, turn_id: &str) -> bool {
        self.budget_limited_turn_ids.remove(turn_id)
    }
}

impl Drop for TurnLifecycleState {
    fn drop(&mut self) {
        self.end_turn_presentation();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_and_finish_update_running_state() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);

        state.start(Instant::now());
        let presentation = state
            .presentation_alive()
            .expect("running turn should expose presentation liveness");
        assert!(state.agent_turn_running);
        assert!(state.agent_turn_started_at.is_some());
        assert!(state.sleep_inhibitor.is_turn_running());

        state.finish();
        assert!(!presentation.load(Ordering::Relaxed));
        assert!(!state.agent_turn_running);
        assert!(state.agent_turn_started_at.is_none());
        assert!(!state.sleep_inhibitor.is_turn_running());
    }

    #[test]
    fn disabling_and_reenabling_presentation_replaces_the_liveness_token() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);
        state.start(Instant::now());
        let first = state
            .presentation_alive()
            .expect("running turn should expose presentation liveness");

        state.set_presentation_enabled(/*enabled*/ false);
        assert!(!first.load(Ordering::Relaxed));
        assert!(state.presentation_alive().is_none());

        state.set_presentation_enabled(/*enabled*/ true);
        let second = state
            .presentation_alive()
            .expect("reenabling presentation should create a fresh token");
        assert!(second.load(Ordering::Relaxed));
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn dropping_turn_lifecycle_invalidates_presentation() {
        let presentation = {
            let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);
            state.start(Instant::now());
            state
                .presentation_alive()
                .expect("running turn should expose presentation liveness")
        };

        assert!(!presentation.load(Ordering::Relaxed));
    }

    #[test]
    fn budget_limited_turn_ids_are_consumed() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);

        state.mark_budget_limited("turn-1".to_string());

        assert!(state.take_budget_limited("turn-1"));
        assert!(!state.take_budget_limited("turn-1"));
    }
}
