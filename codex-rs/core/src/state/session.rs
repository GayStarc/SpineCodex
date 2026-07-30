//! Session-wide mutable state.

use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
// Spine MODIFIED: Import persisted and usage inputs consumed by the session runtime adapter.
// Reason: SessionState is where native history and SDK replay observations meet.
use codex_protocol::protocol::RolloutItem;
use codex_sandboxing::policy_transforms::merge_permission_profiles;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use super::AdditionalContextStore;
use super::auto_compact_window::AutoCompactWindow;
use super::auto_compact_window::AutoCompactWindowIds;
use super::auto_compact_window::AutoCompactWindowPrefillClaim;
#[cfg(test)]
use super::auto_compact_window::AutoCompactWindowSnapshot;
use crate::TurnContext;
use crate::context_manager::ContextManager;
use crate::context_manager::is_model_generated_item;
use crate::session::PreviousTurnSettings;
use crate::session::session::SessionConfiguration;
use crate::session::time_reminder::CurrentTimeReminderState;
use crate::session_startup_prewarm::SessionStartupPrewarmHandle;
// Spine MODIFIED: Import the observer and runtime halves of the private Spine adapter.
// Reason: SessionState owns their lifetime beside the authoritative ContextManager.
use crate::spine::coordinator::SharedSpineCoordinator;
use crate::spine::observer::CodexSpineObserverHandler;
use crate::spine::session_runtime::SessionSpineRuntime;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_output_truncation::TruncationPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectedUsageBasis {
    ProviderValid,
    EstimateCurrent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextPressureSnapshot {
    pub(crate) active_context_tokens: i64,
    pub(crate) body_after_prefix_tokens: i64,
    pub(crate) body_after_prefix_prefill_tokens: Option<i64>,
}
/// Persistent, session-scoped state previously stored directly on `Session`.
pub(crate) struct SessionState {
    pub(crate) session_configuration: SessionConfiguration,
    pub(crate) history: ContextManager,
    pub(crate) latest_rate_limits: Option<RateLimitSnapshot>,
    pub(crate) server_reasoning_included: bool,
    pub(crate) mcp_dependency_prompted: HashSet<String>,
    pub(crate) additional_context: AdditionalContextStore,
    /// Settings used by the latest regular user turn, used for turn-to-turn
    /// model/realtime handling on subsequent regular turns (including full-context
    /// reinjection after resume or `/compact`).
    previous_turn_settings: Option<PreviousTurnSettings>,
    /// Runtime accounting state for the active auto-compaction window.
    auto_compact_window: AutoCompactWindow,
    /// Startup prewarmed session prepared during session initialization.
    pub(crate) startup_prewarm: Option<SessionStartupPrewarmHandle>,
    pub(crate) current_time_reminder: CurrentTimeReminderState,
    pub(crate) active_connector_selection: HashSet<String>,
    pub(crate) pending_session_start_sources: VecDeque<codex_hooks::SessionStartSource>,
    granted_permissions_by_environment_id: HashMap<String, AdditionalPermissionProfile>,
    next_turn_is_first: bool,
    projected_usage_basis: ProjectedUsageBasis,
    projected_usage_model: Option<String>,
    // Spine MODIFIED: Keep optional SDK state colocated with native model history.
    // Reason: Each history mutation can update Spine synchronously under the same state lock.
    spine_runtime: Option<SessionSpineRuntime>,
}

impl SessionState {
    /// Create a new session state mirroring previous `State::default()` semantics.
    #[cfg(test)]
    pub(crate) fn new(session_configuration: SessionConfiguration) -> Self {
        Self::new_with_auto_compact_window_ids(
            session_configuration,
            AutoCompactWindowIds::new_initial(),
            // Spine MODIFIED: Use a no-op observer for lightweight constructors.
            // Reason: Production bootstrap injects the channel-backed observer explicitly.
            CodexSpineObserverHandler::default(),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
        )
    }

    pub(crate) fn new_with_auto_compact_window_ids(
        session_configuration: SessionConfiguration,
        auto_compact_window_ids: AutoCompactWindowIds,
        // Spine MODIFIED: Accept the session-scoped effect handler during state construction.
        // Reason: SDK transitions publish through Host-owned event and filesystem adapters.
        observer: CodexSpineObserverHandler,
        spine_coordinator: SharedSpineCoordinator,
    ) -> Self {
        let history = ContextManager::new();
        // Spine MODIFIED: Instantiate the feature-gated runtime from immutable session config.
        // Reason: Disabled sessions retain base behavior through a None adapter.
        let spine_runtime =
            SessionSpineRuntime::new(&session_configuration, observer, spine_coordinator);
        Self {
            session_configuration,
            history,
            latest_rate_limits: None,
            server_reasoning_included: false,
            mcp_dependency_prompted: HashSet::new(),
            additional_context: AdditionalContextStore::default(),
            previous_turn_settings: None,
            auto_compact_window: AutoCompactWindow::new_with_ids(auto_compact_window_ids),
            startup_prewarm: None,
            current_time_reminder: CurrentTimeReminderState::default(),
            active_connector_selection: HashSet::new(),
            pending_session_start_sources: VecDeque::new(),
            granted_permissions_by_environment_id: HashMap::new(),
            next_turn_is_first: true,
            projected_usage_basis: ProjectedUsageBasis::ProviderValid,
            projected_usage_model: None,
            spine_runtime,
        }
    }

    // History helpers
    pub(crate) fn record_items<I>(&mut self, items: I, policy: TruncationPolicy)
    where
        I: IntoIterator,
        I::Item: std::ops::Deref<Target = ResponseItem>,
    {
        // Spine MODIFIED: During Sampling, forward exactly the native items accepted by history.
        // Reason: The SDK observes append-only source here; context changes wait for PostSampling.
        let start = self.history.raw_items().len();
        self.history.record_items(items, policy);
        if let Some(spine) = &mut self.spine_runtime {
            let appended = self.history.raw_items()[start..].to_vec();
            spine.append_response_items(&appended, &mut self.history);
        }
    }

    // Spine MODIFIED: Pair native history restoration with deterministic SDK rollout recovery.
    // Reason: Resume, fork, and rollback must rebuild archived epochs and the live projection.
    pub(crate) fn replace_history_from_rollout(
        &mut self,
        items: Vec<ResponseItem>,
        reference_context_item: Option<TurnContextItem>,
        rollout_items: &[RolloutItem],
    ) {
        self.replace_history(items, reference_context_item);
        let Some(spine) = &mut self.spine_runtime else {
            return;
        };
        spine.replay(rollout_items, &mut self.history);
    }

    pub(crate) fn replace_history(
        &mut self,
        items: Vec<ResponseItem>,
        reference_context_item: Option<TurnContextItem>,
    ) {
        self.history.replace(items);
        self.history
            .set_reference_context_item(reference_context_item);
        self.auto_compact_window.clear_prefill();
        self.mark_projected_usage_stale();
    }

    // Spine MODIFIED: Forward authoritative token samples into optional Spine pressure state.
    // Reason: Context-pressure projection belongs to the same locked session snapshot.
    pub(crate) fn observe_token_count(&mut self, event: TokenCountEvent) {
        if let Some(spine) = &mut self.spine_runtime {
            spine.observe_token_count(event);
        }
    }

    // Spine MODIFIED: Mark an explicit native history replacement as a Spine compact epoch.
    // Reason: Compact archives the old root and treats installed replacement context as opaque.
    pub(crate) fn compact_spine_live(&mut self) {
        if let Some(spine) = &mut self.spine_runtime {
            spine.compact_live(&mut self.history);
        }
    }

    pub(crate) fn previous_turn_settings(&self) -> Option<PreviousTurnSettings> {
        self.previous_turn_settings.clone()
    }

    pub(crate) fn projected_usage_enabled(&self) -> bool {
        self.session_configuration.spine_jit_enabled()
            || self.session_configuration.spine_trim_enabled()
    }

    pub(crate) fn projected_history_snapshot(&self) -> Option<Vec<ResponseItem>> {
        self.projected_usage_enabled()
            .then(|| self.clone_history().into_raw_items())
    }

    pub(crate) fn reconcile_projected_history(&mut self, before: Option<&[ResponseItem]>) {
        let Some(before) = before else {
            return;
        };
        let after = self.clone_history();
        let after = after.raw_items();
        if !after.starts_with(before) {
            self.mark_projected_usage_stale();
            return;
        }
        if after[before.len()..].iter().any(is_model_generated_item) {
            self.mark_projected_usage_stale();
        }
    }

    pub(crate) fn mark_projected_usage_stale(&mut self) {
        if !self.projected_usage_enabled() {
            return;
        }
        self.projected_usage_basis = ProjectedUsageBasis::EstimateCurrent;
        self.projected_usage_model = None;
    }

    fn mark_projected_usage_valid(&mut self, model: Option<&str>) {
        if self.projected_usage_enabled() {
            self.projected_usage_basis = ProjectedUsageBasis::ProviderValid;
            self.projected_usage_model = model.map(str::to_string);
        }
    }

    pub(crate) fn set_previous_turn_settings(
        &mut self,
        previous_turn_settings: Option<PreviousTurnSettings>,
    ) {
        self.previous_turn_settings = previous_turn_settings;
    }

    pub(crate) fn set_next_turn_is_first(&mut self, value: bool) {
        self.next_turn_is_first = value;
    }

    pub(crate) fn take_next_turn_is_first(&mut self) -> bool {
        let is_first_turn = self.next_turn_is_first;
        self.next_turn_is_first = false;
        is_first_turn
    }

    pub(crate) fn clone_history(&self) -> ContextManager {
        self.history.clone()
    }

    // Spine MODIFIED: Mirror native image sanitization into the SDK's source representation.
    // Reason: Later context materialization must not restore an invalid projected image.
    pub(crate) fn replace_last_turn_images(&mut self, placeholder: &str) -> bool {
        if !self.history.replace_last_turn_images(placeholder) {
            return false;
        }
        if let Some(spine) = &mut self.spine_runtime {
            spine.replace_last_turn_images(placeholder, self.history.history_version());
        }
        self.mark_projected_usage_stale();
        true
    }

    pub(crate) fn validate_spine_trim(
        &self,
        current_call_id: &str,
        request: &spine_core::TrimRequest,
    ) -> Result<(), String> {
        let spine = self
            .spine_runtime
            .as_ref()
            .ok_or_else(|| "Spine trim runtime is unavailable".to_string())?;
        spine.validate_trim(current_call_id, request)
    }

    pub(crate) fn validate_spine_control(&self, tool: spine_core::SpineTool) -> Result<(), String> {
        self.spine_runtime
            .as_ref()
            .ok_or_else(|| "Spine is not enabled for this session".to_string())?
            .validate_control(tool)
    }

    pub(crate) fn validate_spine_trim_request(
        &self,
        request: &spine_core::TrimRequest,
    ) -> Result<(), String> {
        self.spine_runtime
            .as_ref()
            .ok_or_else(|| "Spine trim runtime is unavailable".to_string())?
            .validate_trim_request(request)
    }
    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.history.set_token_info(info);
    }

    pub(crate) fn set_reference_context_item(&mut self, item: Option<TurnContextItem>) {
        self.history.set_reference_context_item(item);
    }

    pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem> {
        self.history.reference_context_item()
    }

    // Token/rate limit helpers
    pub(crate) fn update_token_info_from_non_sampling_usage(
        &mut self,
        usage: &TokenUsage,
        model_context_window: Option<i64>,
    ) {
        self.history.update_token_info(usage, model_context_window);
        self.mark_projected_usage_stale();
    }

    pub(crate) fn update_token_info_from_sampling_usage(
        &mut self,
        usage: &TokenUsage,
        model_context_window: Option<i64>,
        model: &str,
    ) {
        self.history.update_token_info(usage, model_context_window);
        self.mark_projected_usage_valid(Some(model));
    }

    pub(crate) fn begin_auto_compact_window_sampling_request(
        &mut self,
        estimated_input_tokens: i64,
    ) -> Option<AutoCompactWindowPrefillClaim> {
        if !self.projected_usage_enabled() {
            return None;
        }
        self.auto_compact_window
            .begin_projected_sampling_request(estimated_input_tokens)
    }

    pub(crate) fn needs_auto_compact_window_sampling_request_prefill(&self) -> bool {
        self.projected_usage_enabled()
            && self
                .auto_compact_window
                .needs_projected_sampling_request_prefill()
    }

    pub(crate) fn record_auto_compact_window_server_prefill_from_usage(
        &mut self,
        claim: Option<AutoCompactWindowPrefillClaim>,
        usage: &TokenUsage,
        model: &str,
    ) {
        if self.projected_usage_enabled() {
            if let Some(claim) = claim {
                self.auto_compact_window
                    .record_claimed_server_prefill(claim, usage, model);
            }
        } else {
            self.auto_compact_window
                .ensure_server_observed_prefill_from_usage(usage, model);
        }
    }

    pub(crate) fn set_auto_compact_window_estimated_prefill(&mut self, tokens: i64) {
        self.auto_compact_window.set_estimated_prefill(tokens);
    }

    #[cfg(test)]
    pub(crate) fn auto_compact_window_snapshot(&self) -> AutoCompactWindowSnapshot {
        self.auto_compact_window.snapshot()
    }

    pub(crate) fn claim_token_budget_reminder(&mut self) -> bool {
        self.auto_compact_window.claim_token_budget_reminder()
    }

    pub(crate) fn auto_compact_window_number(&self) -> u64 {
        self.auto_compact_window.window_number()
    }

    pub(crate) fn auto_compact_window_ids(&self) -> AutoCompactWindowIds {
        self.auto_compact_window.ids()
    }

    pub(crate) fn restore_auto_compact_window(
        &mut self,
        window_number: u64,
        ids: AutoCompactWindowIds,
    ) {
        self.auto_compact_window.restore(window_number, ids);
    }

    pub(crate) fn next_auto_compact_window(&self) -> (u64, AutoCompactWindowIds) {
        self.auto_compact_window.next()
    }

    pub(crate) fn install_auto_compact_window(
        &mut self,
        window_number: u64,
        ids: AutoCompactWindowIds,
    ) {
        self.auto_compact_window.install(window_number, ids);
    }

    pub(crate) fn clear_auto_compact_window_prefill(&mut self) {
        self.auto_compact_window.clear_prefill();
    }

    pub(crate) fn request_new_context_window(&mut self) {
        self.auto_compact_window.request_new_context_window();
    }

    pub(crate) fn take_new_context_window_request(&mut self) -> bool {
        self.auto_compact_window.take_new_context_window_request()
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.history.token_info()
    }

    pub(crate) fn set_rate_limits(&mut self, snapshot: RateLimitSnapshot) {
        self.latest_rate_limits = Some(merge_rate_limit_fields(
            self.latest_rate_limits.as_ref(),
            snapshot,
        ));
    }

    pub(crate) fn token_info_and_rate_limits(
        &self,
    ) -> (Option<TokenUsageInfo>, Option<RateLimitSnapshot>) {
        (self.token_info(), self.latest_rate_limits.clone())
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        self.history.set_token_usage_full(context_window);
        self.mark_projected_usage_valid(None);
    }

    pub(crate) fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64 {
        if !self.projected_usage_enabled() {
            return self
                .history
                .get_total_token_usage(server_reasoning_included);
        }

        match self.projected_usage_basis {
            ProjectedUsageBasis::ProviderValid => self
                .clone_history()
                .get_total_token_usage(server_reasoning_included),
            ProjectedUsageBasis::EstimateCurrent => {
                self.estimate_projected_context(&self.clone_history())
            }
        }
    }

    pub(crate) fn context_pressure(
        &self,
        server_reasoning_included: bool,
        model: &str,
    ) -> ContextPressureSnapshot {
        if !self.projected_usage_enabled() {
            let active_context_tokens = self
                .history
                .get_total_token_usage(server_reasoning_included);
            let prefill = self.auto_compact_window.basecodex_prefill_input_tokens();
            return ContextPressureSnapshot {
                active_context_tokens,
                body_after_prefix_tokens: active_context_tokens
                    .saturating_sub(prefill.unwrap_or(active_context_tokens)),
                body_after_prefix_prefill_tokens: prefill,
            };
        }

        let projected = self.clone_history();
        let provider_valid = matches!(
            self.projected_usage_basis,
            ProjectedUsageBasis::ProviderValid
        );
        let active_context_tokens = match self.projected_usage_basis {
            ProjectedUsageBasis::ProviderValid => {
                projected.get_total_token_usage(server_reasoning_included)
            }
            ProjectedUsageBasis::EstimateCurrent => self.estimate_projected_context(&projected),
        };
        if provider_valid
            && self.projected_usage_model.as_deref() == Some(model)
            && let Some(prefill) = self
                .auto_compact_window
                .server_prefill_input_tokens_for_model(model)
        {
            return ContextPressureSnapshot {
                active_context_tokens,
                body_after_prefix_tokens: active_context_tokens.saturating_sub(prefill),
                body_after_prefix_prefill_tokens: Some(prefill),
            };
        }

        let estimated_context_tokens = if provider_valid {
            self.estimate_projected_context(&projected)
        } else {
            active_context_tokens
        };
        let prefill = self.auto_compact_window.estimated_prefill_input_tokens();
        ContextPressureSnapshot {
            active_context_tokens,
            body_after_prefix_tokens: estimated_context_tokens
                .saturating_sub(prefill.unwrap_or(estimated_context_tokens)),
            body_after_prefix_prefill_tokens: prefill,
        }
    }

    fn estimate_projected_context(&self, projected: &ContextManager) -> i64 {
        projected
            .estimate_token_count_with_base_instructions(&BaseInstructions {
                text: self.session_configuration.base_instructions().to_string(),
            })
            .unwrap_or(i64::MAX)
    }

    pub(crate) fn estimate_current_context(&self, turn_context: &TurnContext) -> Option<i64> {
        if self.projected_usage_enabled() {
            self.clone_history().estimate_token_count(turn_context)
        } else {
            self.history.estimate_token_count(turn_context)
        }
    }

    pub(crate) fn estimated_tokens_after_last_model_generated_item(&self) -> i64 {
        if self.projected_usage_enabled() {
            self.clone_history()
                .estimated_tokens_after_last_model_generated_item()
        } else {
            self.history
                .estimated_tokens_after_last_model_generated_item()
        }
    }

    pub(crate) fn set_server_reasoning_included(&mut self, included: bool) {
        self.server_reasoning_included = included;
    }

    pub(crate) fn server_reasoning_included(&self) -> bool {
        self.server_reasoning_included
    }

    pub(crate) fn record_mcp_dependency_prompted<I>(&mut self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.mcp_dependency_prompted.extend(names);
    }

    pub(crate) fn mcp_dependency_prompted(&self) -> HashSet<String> {
        self.mcp_dependency_prompted.clone()
    }

    pub(crate) fn set_session_startup_prewarm(
        &mut self,
        startup_prewarm: SessionStartupPrewarmHandle,
    ) {
        self.startup_prewarm = Some(startup_prewarm);
    }

    pub(crate) fn take_session_startup_prewarm(&mut self) -> Option<SessionStartupPrewarmHandle> {
        self.startup_prewarm.take()
    }

    // Adds connector IDs to the active set and returns the merged selection.
    pub(crate) fn merge_connector_selection<I>(&mut self, connector_ids: I) -> HashSet<String>
    where
        I: IntoIterator<Item = String>,
    {
        self.active_connector_selection.extend(connector_ids);
        self.active_connector_selection.clone()
    }

    // Returns the current connector selection tracked on session state.
    pub(crate) fn get_connector_selection(&self) -> HashSet<String> {
        self.active_connector_selection.clone()
    }

    // Removes all currently tracked connector selections.
    pub(crate) fn clear_connector_selection(&mut self) {
        self.active_connector_selection.clear();
    }

    pub(crate) fn queue_pending_session_start_source(
        &mut self,
        value: codex_hooks::SessionStartSource,
    ) {
        self.pending_session_start_sources.push_back(value);
    }

    pub(crate) fn take_pending_session_start_source(
        &mut self,
    ) -> Option<codex_hooks::SessionStartSource> {
        self.pending_session_start_sources.pop_front()
    }

    pub(crate) fn record_granted_permissions(
        &mut self,
        environment_id: &str,
        permissions: AdditionalPermissionProfile,
    ) {
        let granted_permissions = merge_permission_profiles(
            self.granted_permissions_by_environment_id
                .get(environment_id),
            Some(&permissions),
        );
        if let Some(granted_permissions) = granted_permissions {
            self.granted_permissions_by_environment_id
                .insert(environment_id.to_string(), granted_permissions);
        }
    }

    pub(crate) fn granted_permissions(
        &self,
        environment_id: &str,
    ) -> Option<AdditionalPermissionProfile> {
        self.granted_permissions_by_environment_id
            .get(environment_id)
            .cloned()
    }
}

// Sometimes new snapshots don't include credits or plan information.
// Preserve those from the previous snapshot when missing. For `limit_id`, treat
// missing values as the default `"codex"` bucket.
fn merge_rate_limit_fields(
    previous: Option<&RateLimitSnapshot>,
    mut snapshot: RateLimitSnapshot,
) -> RateLimitSnapshot {
    if snapshot.limit_id.is_none() {
        snapshot.limit_id = Some("codex".to_string());
    }
    if snapshot.credits.is_none() {
        snapshot.credits = previous.and_then(|prior| prior.credits.clone());
    }
    if snapshot.individual_limit.is_none() {
        snapshot.individual_limit = previous.and_then(|prior| prior.individual_limit.clone());
    }
    if snapshot.plan_type.is_none() {
        snapshot.plan_type = previous.and_then(|prior| prior.plan_type);
    }
    snapshot
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
