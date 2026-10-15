//! Permitted normalized observations. No raw frame, descriptor or input claim.
use crate::{
    config::Config,
    input::{Change, Contact, ContactId, Device, NormalizedFrame, Source},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    Attached,
    Reconciled,
    MappingChanged,
    Removed,
    Lost,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Frame {
        source: Source,
        at: Duration,
        sequence: u64,
        selectors: Vec<String>,
        changes: Vec<Change>,
        contacts: Vec<Contact>,
        contacts_started: Vec<ContactId>,
        contacts_ended: Vec<ContactId>,
        ended_contacts: Vec<Contact>,
    },
    Reset {
        source: Source,
        at: Duration,
        selectors: Vec<String>,
        reason: ResetReason,
        keys: BTreeSet<u16>,
        contacts: Vec<Contact>,
    },
}
impl Event {
    pub fn validate(&self) -> std::io::Result<()> {
        if self.source().generation == 0
            || self.source().seat.is_empty()
            || self.source().seat.len() > 256
            || self.source().seat.chars().any(char::is_control)
            || self.selectors().is_empty()
            || self.selectors().len() > 256
            || self.selectors().iter().any(|name| {
                name.is_empty() || name.len() > 256 || name.chars().any(char::is_control)
            })
        {
            return Err(std::io::Error::other(
                "invalid normalized input identity or selectors",
            ));
        }
        match self {
            Self::Frame {
                sequence,
                changes,
                contacts,
                contacts_started,
                contacts_ended,
                ended_contacts,
                ..
            } if *sequence == 0
                || changes.len() > 4096
                || contacts.len() > 256
                || contacts_started.len() > 256
                || contacts_ended.len() > 256
                || ended_contacts.len() > 256 =>
            {
                Err(std::io::Error::other("normalized frame exceeds bounds"))
            }
            Self::Reset { keys, contacts, .. } if keys.len() > 4096 || contacts.len() > 256 => {
                Err(std::io::Error::other("normalized reset exceeds bounds"))
            }
            _ => Ok(()),
        }
    }
    pub fn source(&self) -> &Source {
        match self {
            Self::Frame { source, .. } | Self::Reset { source, .. } => source,
        }
    }
    pub fn at(&self) -> Duration {
        match self {
            Self::Frame { at, .. } | Self::Reset { at, .. } => *at,
        }
    }
    pub fn fresh(&self, now: Duration) -> bool {
        now >= self.at() && now - self.at() <= crate::context::SNAPSHOT_MAX_AGE
    }
    pub fn selectors(&self) -> &[String] {
        match self {
            Self::Frame { selectors, .. } | Self::Reset { selectors, .. } => selectors,
        }
    }
    pub(crate) fn selected(config: &Config, device: &Device) -> Vec<String> {
        if !config.policy.allow_input_events
            || device.virtual_device
            || !config.policy.allowed_seats.contains(&device.source.seat)
        {
            return Vec::new();
        }
        config
            .devices
            .iter()
            .filter(|(id, selector)| {
                config.policy.allowed_devices.contains(id) && selector.matches(device)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub(crate) fn frame(config: &Config, device: &Device, frame: NormalizedFrame) -> Option<Self> {
        let selectors = Self::selected(config, device);
        if selectors.is_empty() {
            return None;
        }
        Some(Self::Frame {
            source: frame.raw.source,
            at: frame.raw.at,
            sequence: frame.raw.sequence,
            selectors,
            changes: frame.changes,
            contacts: frame.contacts,
            contacts_started: frame.contacts_started,
            contacts_ended: frame.contacts_ended,
            ended_contacts: frame.ended_contacts,
        })
    }
    pub(crate) fn reset(
        config: &Config,
        device: &Device,
        at: Duration,
        reason: ResetReason,
        keys: &BTreeSet<u16>,
        contacts: &[Contact],
    ) -> Option<Self> {
        let selectors = Self::selected(config, device);
        if selectors.is_empty() {
            return None;
        }
        let clear = matches!(reason, ResetReason::Removed | ResetReason::Lost);
        let mut contacts = if clear { Vec::new() } else { contacts.to_vec() };
        for contact in &mut contacts {
            contact.fresh = false;
        }
        Some(Self::Reset {
            source: device.source.clone(),
            at,
            selectors,
            reason,
            keys: if clear { BTreeSet::new() } else { keys.clone() },
            contacts,
        })
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn permitted_context(
    context: &crate::worker::ProvidedContext,
    seat: &str,
    session: u64,
    now: Duration,
) -> bool {
    context.binding.seat == seat
        && context.binding.kind == crate::context::SessionKind::User
        && context.snapshot.generation == session
        && context.snapshot.state == crate::context::State::Desktop
        && !context.snapshot.secure_overlay
        && now >= context.snapshot.captured_at
        && now - context.snapshot.captured_at <= crate::context::SNAPSHOT_MAX_AGE
}

#[cfg(target_os = "linux")]
pub(crate) fn permitted_subscription(
    config: &Config,
    capabilities: &BTreeSet<crate::protocol::Capability>,
    context: Option<&crate::worker::ProvidedContext>,
    seat: &str,
    session: u64,
    now: Duration,
) -> bool {
    use crate::protocol::Capability;
    config.mode == crate::config::Mode::Observe
        && config.policy.allow_input_events
        && [
            Capability::InputEvents,
            Capability::LuaEvents,
            Capability::Context,
            Capability::Observations,
        ]
        .iter()
        .all(|capability| capabilities.contains(capability))
        && context.is_some_and(|context| permitted_context(context, seat, session, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_metadata_validation_and_freshness_have_explicit_bounds() {
        let mut input = Event::Reset {
            source: Source {
                device: 1,
                generation: 1,
                seat: "seat0".into(),
            },
            at: Duration::from_millis(100),
            selectors: vec!["keyboard".into()],
            reason: ResetReason::Removed,
            keys: BTreeSet::new(),
            contacts: vec![],
        };
        input.validate().unwrap();
        assert!(!input.fresh(Duration::from_millis(99)));
        assert!(input.fresh(Duration::from_millis(350)));
        assert!(!input.fresh(Duration::from_millis(351)));
        if let Event::Reset { source, .. } = &mut input {
            source.seat.push('\n');
        }
        assert!(input.validate().is_err());
    }
}
