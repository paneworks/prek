use super::*;
use crate::{config::Action, keys::SourceKey, protocol::Fence};
mod ordering;
mod projection;

/// Linear configured key work retaining its original route and coordinator fence.
pub struct KeyWork {
    pub(super) fence: Fence,
    pub(super) dispatch: Dispatch,
}

impl ScopedDispatch {
    pub fn into_key_work(mut self) -> Result<KeyWork, Box<Self>> {
        if !self.key_work.as_ref().is_some_and(|work| {
            work.event() == &self.event && work.dispatch.reorder_scope == self.reorder_scope
        }) {
            return Err(Box::new(self));
        }
        Ok(*self.key_work.take().unwrap())
    }
}

impl KeyWork {
    pub fn event(&self) -> &Event {
        &self.dispatch.outcome.event
    }
    pub fn fence(&self) -> Fence {
        self.fence
    }
    pub fn source(&self) -> &Source {
        self.dispatch.outcome.intent.source()
    }
    pub fn position(&self) -> Option<crate::input::InputPosition> {
        self.dispatch.outcome.intent.position()
    }
    pub fn affects(&self, source: &Source) -> bool {
        self.dispatch.outcome.intent.sources().contains(source) || self.source() == source
    }
    pub fn remap_key(&self) -> Option<SourceKey> {
        match (
            &self.dispatch.outcome.intent.rule.action,
            &self.dispatch.outcome.intent.rule.trigger,
        ) {
            (Action::Remap { .. }, Trigger::Key { key }) => Some(SourceKey {
                source: self.source().clone(),
                code: key.0,
            }),
            _ => None,
        }
    }
    pub fn available(&self, routing: &Routing, at: Duration) -> bool {
        routing.engine.recognition_fence() == Some(self.fence)
            && self.dispatch.key_epoch != 0
            && self.dispatch.key_epoch == routing.key_epoch
            && self.dispatch.configuration == routing.engine.generation()
            && self.dispatch.rule_revision == routing.engine.rule_revision()
            && routing.engine.config().rules.iter().any(|rule| {
                rule == &self.dispatch.outcome.intent.rule
                    && rule.enabled
                    && routing.engine.config().mode_active(rule)
            })
            && self
                .dispatch
                .outcome
                .expires
                .is_none_or(|expiry| at < expiry)
            && routing.available(&self.dispatch.outcome.intent, at)
            && self
                .dispatch
                .outcome
                .intent
                .sources()
                .iter()
                .all(|source| routing.engine.source_current(source))
            && self
                .dispatch
                .chain
                .as_ref()
                .is_none_or(|chain| routing.chain_receipt_valid(chain))
    }
    pub fn input_prefix_ready(&self, routing: &Routing) -> bool {
        if let Some(position) = self.scheduling_position() {
            routing.key_position_prefix_ready(position, self.observe(), &self.source().seat)
        } else {
            routing.key_prefix_ready(&self.dispatch.inputs, self.observe())
        }
    }
    pub fn scheduling_position(&self) -> Option<crate::input::InputPosition> {
        self.dispatch.position
    }
    pub fn blocks_raw_span(&self, span: crate::router::InputSpan) -> bool {
        self.scheduling_position()
            .is_some_and(|position| span.last > position)
    }
    pub fn blocks_raw(&self, source: &Source, sequence: u64) -> bool {
        self.dispatch
            .inputs
            .get(source)
            .is_some_and(|bound| sequence > *bound || sequence == *bound && !self.observe())
    }
    pub(crate) fn observe(&self) -> bool {
        self.dispatch.outcome.intent.rule.delivery == crate::config::Delivery::Observe
    }
    pub(crate) fn inputs(&self) -> &BTreeMap<Source, u64> {
        &self.dispatch.inputs
    }
    pub(crate) fn encoded_bytes(&self) -> io::Result<usize> {
        serde_json::to_vec(self.event())
            .map(|bytes| bytes.len())
            .map_err(io::Error::other)
    }
}

pub(super) fn projectable(config: &Config, device: &Device) -> bool {
    !device.capabilities.keys().any(|kind| {
        matches!(
            *kind,
            crate::input::EV_REL | crate::input::EV_ABS | crate::input::EV_SW
        )
    }) && config
        .rules
        .iter()
        .filter(|rule| rule.enabled && rule.delivery != crate::config::Delivery::Observe)
        .filter(|rule| {
            (rule.seats.is_empty() || rule.seats.contains(&device.source.seat))
                && config.devices.iter().any(|(id, selector)| {
                    (rule.devices.is_empty() || rule.devices.contains(id))
                        && selector.matches(device)
                })
        })
        .all(|rule| {
            matches!(rule.trigger, Trigger::Key { .. })
                && matches!(rule.action, Action::Keys { .. } | Action::Remap { .. })
        })
}

impl Routing {
    pub(crate) fn key_output_available(&mut self, available: bool) {
        if available && !self.capabilities.contains(&Capability::SyntheticInput) {
            self.advance_key_epoch();
            self.capabilities.insert(Capability::SyntheticInput);
        } else if !available {
            self.revoke_key_work();
        }
    }
    pub(crate) fn revoke_key_work(&mut self) {
        let mut capabilities = self.capabilities.clone();
        capabilities.remove(&Capability::SyntheticInput);
        self.restrict_actions(&capabilities);
    }
    pub(crate) fn key_lane(&self, fence: Fence, at: Duration) -> bool {
        self.key_coordinator(fence)
            && self.key_epoch != 0
            && self.capability_generation == self.authority_generation
            && at < self.capability_expires
            && self.capabilities.contains(&Capability::SyntheticInput)
    }
    pub(crate) fn key_coordinator(&self, fence: Fence) -> bool {
        self.engine.recognition_fence() == Some(fence)
    }
    pub(crate) fn key_prefix_ready(&self, inputs: &BTreeMap<Source, u64>, inclusive: bool) -> bool {
        inputs.iter().all(|(source, sequence)| {
            self.routes
                .get(source)
                .is_none_or(|route| route.router.prefix_delivered(*sequence, inclusive))
        })
    }
}

#[cfg(test)]
mod tests;
