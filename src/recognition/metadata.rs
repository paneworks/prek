//! Declaration-scoped device metadata without held input state.
use super::*;
use crate::{
    config::Config,
    input_events::{DeviceInfo, ResetReason},
};

pub(crate) fn metadata_selectors(config: &Config) -> BTreeSet<String> {
    config
        .recognizer_definitions()
        .into_iter()
        .flat_map(|definition| definition.selectors)
        .collect()
}

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub source: Source,
    pub at: Duration,
    pub selectors: Vec<String>,
    pub reason: ResetReason,
    pub metadata: Option<Box<DeviceInfo>>,
}
impl Metadata {
    pub(crate) fn project(input: Event, scope: &BTreeSet<String>) -> io::Result<Option<Self>> {
        let Event::Reset {
            source,
            at,
            selectors,
            reason,
            mut metadata,
            ..
        } = input
        else {
            return Err(invalid("recognizer metadata requires a native reset"));
        };
        let selectors: Vec<_> = selectors
            .into_iter()
            .filter(|id| scope.contains(id))
            .collect();
        if selectors.is_empty() {
            return Ok(None);
        }
        if let Some(info) = &mut metadata {
            info.selectors = selectors.clone();
        }
        let result = Self {
            source,
            at,
            selectors,
            reason,
            metadata,
        };
        Ok(Some(result))
    }
    pub fn validate(&self) -> io::Result<()> {
        Event::Reset {
            source: self.source.clone(),
            at: self.at,
            selectors: self.selectors.clone(),
            reason: self.reason,
            keys: BTreeSet::new(),
            contacts: vec![],
            metadata: self.metadata.clone(),
        }
        .validate()?;
        if self.selectors.windows(2).any(|pair| pair[0] >= pair[1])
            || (self.reason == ResetReason::Removed) != self.metadata.is_none()
        {
            return Err(invalid("invalid recognizer metadata shape"));
        }
        encoded_size(self, MAX_FRAME_BYTES)?;
        Ok(())
    }
    pub fn fresh(&self, now: Duration) -> bool {
        now >= self.at && now - self.at <= crate::context::SNAPSHOT_MAX_AGE
    }
    pub(crate) fn validate_for(
        &self,
        config: &Config,
        scope: &BTreeSet<String>,
        now: Duration,
    ) -> io::Result<()> {
        self.validate()?;
        if !self.fresh(now)
            || !config.policy.allow_input_events
            || !config.policy.allowed_seats.contains(&self.source.seat)
            || self
                .selectors
                .iter()
                .any(|id| !scope.contains(id) || !config.policy.allowed_devices.contains(id))
            || self.metadata.as_ref().is_some_and(|info| {
                Event::selected(config, &info.device)
                    .into_iter()
                    .filter(|id| scope.contains(id))
                    .collect::<Vec<_>>()
                    != self.selectors
            })
        {
            return Err(invalid(
                "recognizer metadata exceeds current declaration scope or freshness",
            ));
        }
        Ok(())
    }
}
