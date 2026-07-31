use codex_protocol::protocol::TokenUsage;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoCompactWindowIds {
    pub(crate) first_window_id: Uuid,
    pub(crate) previous_window_id: Option<Uuid>,
    pub(crate) window_id: Uuid,
}

impl AutoCompactWindowIds {
    pub(crate) fn new_initial() -> Self {
        let window_id = Uuid::now_v7();
        Self {
            first_window_id: window_id,
            previous_window_id: None,
            window_id,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoCompactWindowSnapshot {
    pub(crate) estimated_prefill_input_tokens: Option<i64>,
    pub(crate) server_prefill_input_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoCompactWindowPrefillClaim {
    window_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerObservedPrefill {
    input_tokens: i64,
    model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AutoCompactWindow {
    window_number: u64,
    ids: AutoCompactWindowIds,
    new_context_window_requested: bool,
    /// Local estimate of the first admitted sampling request in this window.
    ///
    /// This remains available when projection rewrites make provider usage
    /// stale, so BodyAfterPrefix always subtracts values in one coordinate.
    estimated_prefill_input_tokens: Option<i64>,
    /// Provider input usage for the same first admitted sampling request.
    server_prefill: Option<ServerObservedPrefill>,
    /// Once the first sampling request is bound, later requests must not claim
    /// its provider baseline even if the first response omitted usage.
    first_sampling_request_bound: bool,
    token_budget_reminder_delivered: bool,
}

impl AutoCompactWindow {
    pub(super) fn new_with_ids(ids: AutoCompactWindowIds) -> Self {
        Self {
            window_number: 0,
            ids,
            new_context_window_requested: false,
            estimated_prefill_input_tokens: None,
            server_prefill: None,
            first_sampling_request_bound: false,
            token_budget_reminder_delivered: false,
        }
    }

    pub(super) fn clear_prefill(&mut self) {
        self.estimated_prefill_input_tokens = None;
        self.server_prefill = None;
        self.first_sampling_request_bound = false;
    }

    pub(super) fn window_number(&self) -> u64 {
        self.window_number
    }

    pub(super) fn ids(&self) -> AutoCompactWindowIds {
        self.ids
    }

    pub(super) fn restore(&mut self, window_number: u64, ids: AutoCompactWindowIds) {
        self.window_number = window_number;
        self.ids = ids;
    }

    pub(super) fn next(&self) -> (u64, AutoCompactWindowIds) {
        let mut ids = self.ids;
        ids.previous_window_id = Some(ids.window_id);
        ids.window_id = Uuid::now_v7();
        (self.window_number.saturating_add(1), ids)
    }

    pub(super) fn install(&mut self, window_number: u64, ids: AutoCompactWindowIds) {
        self.window_number = window_number;
        self.ids = ids;
        self.new_context_window_requested = false;
        self.token_budget_reminder_delivered = false;
    }

    pub(super) fn claim_token_budget_reminder(&mut self) -> bool {
        !std::mem::replace(&mut self.token_budget_reminder_delivered, true)
    }

    pub(super) fn request_new_context_window(&mut self) {
        self.new_context_window_requested = true;
    }

    pub(super) fn take_new_context_window_request(&mut self) -> bool {
        let requested = self.new_context_window_requested;
        self.new_context_window_requested = false;
        requested
    }

    pub(super) fn begin_projected_sampling_request(
        &mut self,
        estimated_input_tokens: i64,
    ) -> Option<AutoCompactWindowPrefillClaim> {
        if self.first_sampling_request_bound {
            return None;
        }

        self.estimated_prefill_input_tokens = Some(estimated_input_tokens.max(0));
        self.server_prefill = None;
        self.first_sampling_request_bound = true;
        Some(AutoCompactWindowPrefillClaim {
            window_id: self.ids.window_id,
        })
    }

    pub(super) fn needs_projected_sampling_request_prefill(&self) -> bool {
        !self.first_sampling_request_bound
    }

    pub(super) fn record_claimed_server_prefill(
        &mut self,
        claim: AutoCompactWindowPrefillClaim,
        usage: &TokenUsage,
        model: &str,
    ) {
        if claim.window_id != self.ids.window_id
            || !self.first_sampling_request_bound
            || self.server_prefill.is_some()
        {
            return;
        }

        self.server_prefill = Some(ServerObservedPrefill {
            input_tokens: usage.input_tokens.max(0),
            model: model.to_string(),
        });
    }

    /// Preserves BaseCodex's feature-off behavior: the first observed provider
    /// usage replaces a provisional estimated prefill.
    pub(super) fn ensure_server_observed_prefill_from_usage(
        &mut self,
        usage: &TokenUsage,
        model: &str,
    ) {
        if self.server_prefill.is_some() {
            return;
        }

        self.server_prefill = Some(ServerObservedPrefill {
            input_tokens: usage.input_tokens.max(0),
            model: model.to_string(),
        });
    }

    pub(super) fn set_estimated_prefill(&mut self, tokens: i64) {
        if self.first_sampling_request_bound || self.server_prefill.is_some() {
            return;
        }

        self.estimated_prefill_input_tokens = Some(tokens.max(0));
    }

    pub(super) fn estimated_prefill_input_tokens(&self) -> Option<i64> {
        self.estimated_prefill_input_tokens
    }

    pub(super) fn server_prefill_input_tokens_for_model(&self, model: &str) -> Option<i64> {
        self.server_prefill
            .as_ref()
            .filter(|prefill| prefill.model == model)
            .map(|prefill| prefill.input_tokens)
    }

    pub(super) fn basecodex_prefill_input_tokens(&self) -> Option<i64> {
        self.server_prefill
            .as_ref()
            .map(|prefill| prefill.input_tokens)
            .or(self.estimated_prefill_input_tokens)
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> AutoCompactWindowSnapshot {
        AutoCompactWindowSnapshot {
            estimated_prefill_input_tokens: self.estimated_prefill_input_tokens,
            server_prefill_input_tokens: self
                .server_prefill
                .as_ref()
                .map(|prefill| prefill.input_tokens),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn tracks_prefill_and_window_boundaries() {
        let mut window = AutoCompactWindow::new_with_ids(AutoCompactWindowIds::new_initial());

        assert_eq!(window.window_number(), 0);
        let initial_window_id = window.ids().window_id;
        assert_eq!(initial_window_id.get_version_num(), 7);
        assert_eq!(
            window.ids(),
            AutoCompactWindowIds {
                first_window_id: initial_window_id,
                previous_window_id: None,
                window_id: initial_window_id,
            }
        );
        let first_window_id = initial_window_id;
        let restored_window_id = Uuid::now_v7();
        let restored_previous_window_id = Uuid::now_v7();
        window.restore(
            /*window_number*/ 3,
            AutoCompactWindowIds {
                first_window_id,
                previous_window_id: Some(restored_previous_window_id),
                window_id: restored_window_id,
            },
        );
        assert_eq!(window.window_number(), 3);
        assert_eq!(window.ids().window_id, restored_window_id);
        assert!(window.claim_token_budget_reminder());
        assert!(!window.claim_token_budget_reminder());
        window.request_new_context_window();
        assert!(window.take_new_context_window_request());
        assert!(!window.take_new_context_window_request());
        window.request_new_context_window();
        let (window_number, ids) = window.next();
        assert_eq!(window_number, 4);
        assert_eq!(window.window_number(), 3);
        assert_eq!(window.ids().window_id, restored_window_id);
        window.install(window_number, ids);
        assert_eq!(window.window_number(), 4);
        assert_eq!(window.ids(), ids);
        assert_eq!(ids.first_window_id, first_window_id);
        assert_eq!(ids.previous_window_id, Some(restored_window_id));
        assert_eq!(ids.window_id.get_version_num(), 7);
        assert_ne!(ids.window_id, restored_window_id);
        assert!(!window.take_new_context_window_request());
        assert!(window.claim_token_budget_reminder());

        assert_eq!(
            window.snapshot(),
            AutoCompactWindowSnapshot {
                estimated_prefill_input_tokens: None,
                server_prefill_input_tokens: None,
            }
        );

        window.set_estimated_prefill(/*tokens*/ 150);
        assert_eq!(
            window.snapshot(),
            AutoCompactWindowSnapshot {
                estimated_prefill_input_tokens: Some(150),
                server_prefill_input_tokens: None,
            }
        );

        let claim = window
            .begin_projected_sampling_request(/*estimated_input_tokens*/ 140)
            .expect("first sampling request should claim the window prefill");
        window.record_claimed_server_prefill(
            claim,
            &TokenUsage {
                input_tokens: 120,
                total_tokens: 170,
                ..Default::default()
            },
            "gpt-test",
        );
        assert_eq!(
            window.snapshot(),
            AutoCompactWindowSnapshot {
                estimated_prefill_input_tokens: Some(140),
                server_prefill_input_tokens: Some(120),
            }
        );
        assert_eq!(
            window.server_prefill_input_tokens_for_model("gpt-test"),
            Some(120)
        );
        assert_eq!(
            window.server_prefill_input_tokens_for_model("other-model"),
            None
        );
        assert!(
            window
                .begin_projected_sampling_request(/*estimated_input_tokens*/ 90)
                .is_none()
        );

        window.ensure_server_observed_prefill_from_usage(
            &TokenUsage {
                input_tokens: 130,
                total_tokens: 180,
                ..Default::default()
            },
            "gpt-test",
        );
        window.set_estimated_prefill(/*tokens*/ 90);
        assert_eq!(
            window.snapshot(),
            AutoCompactWindowSnapshot {
                estimated_prefill_input_tokens: Some(140),
                server_prefill_input_tokens: Some(120),
            }
        );

        let (window_number, ids) = window.next();
        assert_ne!(ids.window_id, restored_window_id);
        assert_eq!(
            window.snapshot(),
            AutoCompactWindowSnapshot {
                estimated_prefill_input_tokens: Some(140),
                server_prefill_input_tokens: Some(120),
            },
            "preparing the next window must not rebase the live window"
        );
        window.install(window_number, ids);
        window.clear_prefill();
        assert_eq!(
            window.snapshot(),
            AutoCompactWindowSnapshot {
                estimated_prefill_input_tokens: None,
                server_prefill_input_tokens: None,
            }
        );
        window.record_claimed_server_prefill(
            claim,
            &TokenUsage {
                input_tokens: 999,
                total_tokens: 1_000,
                ..Default::default()
            },
            "gpt-test",
        );
        assert_eq!(
            window.snapshot(),
            AutoCompactWindowSnapshot {
                estimated_prefill_input_tokens: None,
                server_prefill_input_tokens: None,
            },
            "a claim from the previous window must not install a delayed provider baseline"
        );
        window.ensure_server_observed_prefill_from_usage(
            &TokenUsage {
                input_tokens: 120,
                total_tokens: 170,
                ..Default::default()
            },
            "base-model",
        );
        assert_eq!(
            window.snapshot(),
            AutoCompactWindowSnapshot {
                estimated_prefill_input_tokens: None,
                server_prefill_input_tokens: Some(120),
            }
        );
    }
}
