//! Private broker/worker admission for native Morf ownership. These messages
//! carry authority only on the authenticated supervisor channel.
use crate::{input::Source, protocol::Fence, supervisor::WorkerIdentity};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[cfg(test)]
mod tests;

pub const FRESHNESS: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub output: String,
    pub epoch: String,
    pub ui_generation: u64,
    pub ownership_generation: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    pub owners: Vec<Owner>,
    pub expires: Option<Duration>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub worker: WorkerIdentity,
    pub fence: Fence,
    pub source: Source,
    pub attempt: u64,
    pub observed_at: Duration,
    pub owners: Vec<Owner>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub activation: Activation,
    pub expires: Duration,
}

pub(crate) fn valid_owners(owners: &[Owner]) -> bool {
    !owners.is_empty()
        && owners.len() <= 8
        && owners
            .windows(2)
            .all(|pair| pair[0].output < pair[1].output)
        && owners.iter().all(|owner| {
            !owner.output.is_empty()
                && owner.output.len() <= 256
                && !owner.output.chars().any(char::is_control)
                && owner.ui_generation > 0
                && owner.ownership_generation > 0
                && owner.epoch.len() == 32
                && owner
                    .epoch
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}
impl Activation {
    pub(crate) fn fresh(&self, now: Duration) -> bool {
        self.fence.validate().is_ok()
            && self.worker.valid()
            && self.source.generation > 0
            && !self.source.seat.is_empty()
            && self.source.seat.len() <= 256
            && !self.source.seat.chars().any(char::is_control)
            && self.attempt > 0
            && now >= self.observed_at
            && now - self.observed_at < FRESHNESS
            && valid_owners(&self.owners)
    }
}

/// One prepared source. Consumer readiness cannot set `authorized`; only the
/// reply to this source's current neutral observation can do so.
pub(crate) struct Gate {
    worker: WorkerIdentity,
    fence: Fence,
    source: Source,
    learned: bool,
    owners: Vec<Owner>,
    next: u64,
    pending: Option<Activation>,
    authorized: bool,
    expires: Option<Duration>,
}
impl Gate {
    pub fn new(worker: WorkerIdentity, fence: Fence, source: Source) -> Self {
        Self {
            worker,
            fence,
            source,
            learned: false,
            owners: Vec::new(),
            next: 0,
            pending: None,
            authorized: false,
            expires: None,
        }
    }
    pub fn update(
        &mut self,
        admission: Option<Admission>,
        now: Duration,
    ) -> Result<(), &'static str> {
        if admission
            .as_ref()
            .is_some_and(|admission| admission.owners.is_empty())
        {
            return Err("empty native ownership requirements");
        }
        let (owners, mut expires) = admission.map_or((Vec::new(), None), |admission| {
            (admission.owners, admission.expires)
        });
        if !owners.is_empty() && !valid_owners(&owners) {
            return Err("invalid native ownership requirements");
        }
        if self.learned && owners != self.owners {
            return Err("native ownership requirements changed within a source lifetime");
        }
        if let Some(expires) = expires
            && (owners.is_empty() || expires > now && expires - now > FRESHNESS)
        {
            return Err("invalid native ownership lease");
        }
        if expires.is_some_and(|until| until <= now) {
            expires = None;
        }
        self.learned = true;
        self.owners = owners;
        if self.authorized && self.expires.is_none_or(|until| now >= until) {
            self.authorized = false;
            self.pending = None;
        }
        // A renewal can extend only an already granted source. An initial
        // transport response cannot bypass its neutral activation handshake.
        if self.authorized {
            self.expires = expires;
        }
        Ok(())
    }
    pub fn watched(&self) -> bool {
        !self.owners.is_empty()
    }
    pub fn allowed(&self, now: Duration) -> bool {
        self.learned
            && (!self.watched() || self.authorized && self.expires.is_some_and(|until| now < until))
    }
    pub fn request(&mut self, now: Duration) -> Result<Option<Activation>, &'static str> {
        if !self.learned || !self.watched() || self.allowed(now) {
            return Ok(None);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.fresh(now))
        {
            return Ok(None);
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("native admission attempt exhausted")?;
        let activation = Activation {
            worker: self.worker,
            fence: self.fence,
            source: self.source.clone(),
            attempt: self.next,
            observed_at: now,
            owners: self.owners.clone(),
        };
        self.pending = Some(activation.clone());
        Ok(Some(activation))
    }
    pub fn cancel_pending(&mut self) {
        self.pending = None;
        if self.watched() {
            self.authorized = false;
            self.expires = None;
        }
    }
    pub fn grant(&mut self, grant: &Grant, now: Duration) -> bool {
        if self.pending.as_ref() != Some(&grant.activation)
            || !grant.activation.fresh(now)
            || grant.expires <= now
            || grant.expires > grant.activation.observed_at + FRESHNESS
        {
            return false;
        }
        self.pending = None;
        self.authorized = true;
        self.expires = Some(grant.expires);
        true
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.expires
    }
}
