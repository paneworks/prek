//! Morf's per-output global-phone-touch handoff. Native/UI acknowledgement is
//! separate from the broker's physical-device readiness and descriptor proof.
use super::*;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum HandoffPhase {
    Native,
    Quiescing,
    Quiesced,
    Owned,
    Releasing,
    Lost,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HandoffState {
    version: u32,
    epoch: String,
    ui_generation: Decimal,
    handoff: State,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct State {
    phase: HandoffPhase,
    ownership_generation: Decimal,
    global_enabled: bool,
    native_neutral: bool,
    contacts: u32,
    contact_revision: Decimal,
    ui_ack: bool,
    driver: String,
    observed_ns: Decimal,
    scope: String,
    recognizer_ownership: bool,
}
impl HandoffState {
    pub fn phase(&self) -> HandoffPhase {
        self.handoff.phase
    }
    pub fn ownership_generation(&self) -> u64 {
        self.handoff.ownership_generation.0
    }
    pub fn ui_generation(&self) -> u64 {
        self.ui_generation.0
    }
    pub fn global_enabled(&self) -> bool {
        self.handoff.global_enabled
    }
    pub fn contacts(&self) -> u32 {
        self.handoff.contacts
    }
    pub fn ready_to_activate(&self, now: Duration) -> bool {
        let observed = Duration::from_nanos(self.handoff.observed_ns.0);
        self.phase() == HandoffPhase::Quiesced
            && self.handoff.ui_ack
            && self.handoff.native_neutral
            && self.handoff.contacts == 0
            && self.handoff.driver == "native"
            && now >= observed
            && now - observed < GRANT
    }
    fn validate(&self, state: &ShellState, previous: Option<u64>, now: Duration) -> io::Result<()> {
        let h = &self.handoff;
        if self.version != 1
            || self.epoch != state.epoch
            || self.ui_generation.0 == 0
            || previous.is_some_and(|old| self.ui_generation.0 < old)
            || h.scope != "global_phone_touch"
            || h.recognizer_ownership
            || h.contacts > 64
            || h.native_neutral && h.contacts != 0
            || h.global_enabled != (h.phase == HandoffPhase::Native)
            || Duration::from_nanos(h.observed_ns.0) > now
            || !matches!(h.driver.as_str(), "" | "native" | "lisgd" | "unavailable")
            || matches!(
                h.phase,
                HandoffPhase::Quiescing
                    | HandoffPhase::Quiesced
                    | HandoffPhase::Owned
                    | HandoffPhase::Releasing
            ) && h.ownership_generation.0 == 0
            || matches!(h.phase, HandoffPhase::Quiesced | HandoffPhase::Owned)
                && (!h.ui_ack || h.driver != "native" || h.observed_ns.0 == 0)
            || h.phase == HandoffPhase::Quiesced && !h.native_neutral
        {
            return Err(invalid("invalid native handoff state or binding"));
        }
        Ok(())
    }
}

impl Client<ShellState> {
    /// Read-only state. A query also asks the native UI loop for a fresh
    /// contact observation; poll read-only state while waiting for quiescence.
    pub fn handoff_state(
        &mut self,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> io::Result<HandoffState> {
        let until = Instant::now() + RPC_TIME;
        let result = (|| {
            let state = self.sample_until(logind, compositor, until)?;
            let bytes = self.transport.exchange(
                b"{\"version\":1,\"op\":\"handoff-state\"}\n",
                until,
                &mut false,
            )?;
            let handoff: HandoffState = serde_json::from_slice(&bytes)?;
            handoff.validate(&state, self.control_generation, monotonic()?)?;
            let after = self.sample_until(logind, compositor, until)?;
            handoff.validate(&after, self.control_generation, monotonic()?)?;
            self.control_generation = Some(handoff.ui_generation());
            Ok(handoff)
        })();
        if result.is_err() {
            self.transport.retire();
        }
        result
    }
    /// Requests inhibition, not completed takeover. Wait for a fresh quiesced
    /// state before activation; an unmanaged external lisgd cannot acknowledge it.
    pub fn quiesce_handoff(
        &mut self,
        scope: Scope<'_>,
        generation: u64,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        self.control(
            scope,
            Operation::HandoffQuiesce {
                ownership_generation: Decimal(generation),
            },
            logind,
            compositor,
        )
    }
    /// Activates only the exact quiesced VM/ownership generation. Physical
    /// readiness, original-device neutral state and grabs remain broker duties.
    pub fn activate_handoff(
        &mut self,
        scope: Scope<'_>,
        state: &HandoffState,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        let valid = (|| {
            if state.epoch != scope.capabilities.epoch
                || state.ui_generation() != scope.capabilities.ui_generation()
                || !state.ready_to_activate(monotonic()?)
            {
                return Err(denied("handoff activation needs fresh exact quiescence"));
            }
            Ok(())
        })();
        valid.map_err(|source| Error {
            source,
            uncertain: false,
        })?;
        self.control(
            scope,
            Operation::HandoffActivate {
                ownership_generation: Decimal(state.ownership_generation()),
            },
            logind,
            compositor,
        )
    }
    /// Root's release assertion. The caller must first cancel recognition,
    /// drain input and confirm every owned evdev/uinput descriptor is closed.
    /// Transport alone supplies no such proof and never releases on Drop.
    pub fn confirm_handoff_release(
        &mut self,
        scope: Scope<'_>,
        generation: u64,
        logind: &Logind,
        compositor: &hyprland::Client,
    ) -> Result<Reply, Error> {
        self.control(
            scope,
            Operation::HandoffRelease {
                ownership_generation: Decimal(generation),
                descriptors_released: true,
            },
            logind,
            compositor,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn value() -> serde_json::Value {
        json!({"version":1,"epoch":"a".repeat(32),"ui_generation":"1","handoff":{
            "phase":"quiesced","ownership_generation":"1","global_enabled":false,"native_neutral":true,
            "contacts":0,"contact_revision":"2","ui_ack":true,"driver":"native","observed_ns":"1000000",
            "scope":"global_phone_touch","recognizer_ownership":false}})
    }
    fn binding() -> ShellState {
        ShellState {
            version: 1,
            role: "shell".into(),
            epoch: "a".repeat(32),
            revision: 2,
            phase: ShellPhase::Configured,
            compositor_pid: 1,
            compositor_start: 1,
            compositor_uid: 1000,
            output: "HEADLESS-1".into(),
        }
    }
    #[test]
    fn handoff_requires_correlated_native_neutral_ui_ack_and_bounded_freshness() {
        let state: HandoffState = serde_json::from_value(value()).unwrap();
        state
            .validate(&binding(), Some(1), Duration::from_millis(1))
            .unwrap();
        assert!(state.ready_to_activate(Duration::from_millis(250)));
        assert!(!state.ready_to_activate(Duration::from_millis(251)));
        for (field, bad) in [
            ("scope", json!("all_input")),
            ("global_enabled", json!(true)),
            ("ui_ack", json!(false)),
            ("native_neutral", json!(false)),
            ("contacts", json!(1)),
            ("recognizer_ownership", json!(true)),
            ("driver", json!("lisgd")),
            ("ownership_generation", json!("0")),
            ("observed_ns", json!("2000000")),
        ] {
            let mut changed = value();
            changed["handoff"][field] = bad;
            assert!(
                serde_json::from_value::<HandoffState>(changed)
                    .unwrap()
                    .validate(&binding(), Some(1), Duration::from_millis(1))
                    .is_err(),
                "{field}"
            );
        }
        assert!(
            state
                .validate(&binding(), Some(2), Duration::from_millis(1))
                .is_err()
        );
    }
}
