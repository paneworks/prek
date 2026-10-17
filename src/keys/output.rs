//! Reference-counted key emission through an owned nonblocking frame sink.
//! Authorization and consumer readiness remain the caller's responsibility.
//! Retire and flush before dropping; descriptor removal is not a consumer-neutral receipt.

use super::{LogicalKeys, Owner};
use crate::input::{EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT, Source};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::Duration,
};

const MAX_OWNERS: usize = 4096;
const MAX_KEYS: usize = 32;
const MAX_SOURCES: usize = 4096;
const MAX_RECEIPT_GENERATIONS: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    Pending,
    Complete,
}

pub trait Sink {
    fn source(&self) -> &Source;
    fn supports_key(&self, code: u16) -> bool;
    fn enqueue(&mut self, frame: &Frame) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<Progress>;
    /// Discard the unwritten tail and return the exact successfully written prefix.
    fn abort(&mut self) -> io::Result<Vec<RawEvent>>;
}

/// Generated owner IDs are increasing output serials within each configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Claim {
    pub owner: Owner,
    pub origin: Option<Source>,
    pub configuration: u64,
}

struct Held {
    claim: Claim,
    keys: Vec<u16>,
}

enum Mutation {
    None,
    Repeat(Owner),
    Add(Held),
    Remove(Vec<Owner>),
}

struct Pending {
    frame: Frame,
    mutation: Mutation,
    release_after: Option<Owner>,
}

pub struct Output<S: Sink> {
    sink: Option<S>,
    source: Source,
    logical: LogicalKeys,
    held: BTreeMap<Owner, Held>,
    receipts: BTreeMap<u64, u64>,
    retired_sources: BTreeMap<u64, u64>,
    retired_generation: u64,
    closing: bool,
    failed: bool,
    pending: Option<Pending>,
    sequence: u64,
    at: Duration,
}

impl<S: Sink> Output<S> {
    pub fn new(sink: S) -> io::Result<Self> {
        let source = sink.source().clone();
        validate_source(&source)?;
        Ok(Self {
            sink: Some(sink),
            source,
            logical: LogicalKeys::default(),
            held: BTreeMap::new(),
            receipts: BTreeMap::new(),
            retired_sources: BTreeMap::new(),
            retired_generation: 0,
            closing: false,
            failed: false,
            pending: None,
            sequence: 0,
            at: Duration::ZERO,
        })
    }

    /// Hold keys in their supplied order until explicit release or retirement.
    pub fn hold(&mut self, claim: Claim, keys: &[u16], at: Duration) -> io::Result<Progress> {
        self.add(claim, keys, at, false)
    }

    /// Emit a synthetic chord, releasing its keys in reverse order after its press commits.
    pub fn tap(&mut self, claim: Claim, keys: &[u16], at: Duration) -> io::Result<Progress> {
        if !matches!(claim.owner, Owner::Synthetic { .. }) {
            return Err(invalid("a key chord requires a synthetic owner"));
        }
        self.add(claim, keys, at, true)
    }

    fn add(&mut self, claim: Claim, keys: &[u16], at: Duration, tap: bool) -> io::Result<Progress> {
        self.ready(at)?;
        self.validate_claim(&claim)?;
        if self.closing || self.cancelled(&claim) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "key owner retired",
            ));
        }
        if keys.is_empty()
            || keys.len() > MAX_KEYS
            || keys.iter().collect::<BTreeSet<_>>().len() != keys.len()
            || matches!(claim.owner, Owner::Physical(_)) && keys.len() != 1
            || keys.iter().any(|&key| {
                !(1..768).contains(&key) || !self.sink.as_ref().unwrap().supports_key(key)
            })
        {
            return Err(invalid(
                "key output requires 1..32 distinct supported key codes",
            ));
        }
        if let Some(old) = self.held.get(&claim.owner) {
            if !tap && old.claim == claim && old.keys == keys {
                self.at = at;
                return Ok(Progress::Complete);
            }
            return Err(invalid("key owner is already bound"));
        }
        if matches!(claim.owner, Owner::Synthetic { id, .. } if self.receipts.get(&claim.configuration).is_some_and(|last| id <= *last))
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "synthetic key output serial is stale or already accepted",
            ));
        }
        if self.held.len() >= MAX_OWNERS
            || matches!(claim.owner, Owner::Synthetic { .. })
                && !self.receipts.contains_key(&claim.configuration)
                && self.receipts.len() >= MAX_RECEIPT_GENERATIONS
        {
            return Err(invalid("key output owner or action receipt limit exceeded"));
        }
        let events = keys
            .iter()
            .filter(|&&key| !self.logical.down(&self.source.seat, key))
            .map(|&code| edge(code, true))
            .collect();
        let release_after = tap.then(|| claim.owner.clone());
        self.at = at;
        self.stage(
            Mutation::Add(Held {
                claim,
                keys: keys.to_vec(),
            }),
            events,
            release_after,
        )
    }

    pub fn release(&mut self, claim: &Claim, at: Duration) -> io::Result<Progress> {
        self.ready(self.at)?;
        self.validate_binding(claim)?;
        self.at = self.at.max(at);
        self.remove(vec![claim.owner.clone()])
    }

    /// Forward a held remap's repeat without creating another owner or press.
    pub fn repeat(&mut self, claim: &Claim, at: Duration) -> io::Result<Progress> {
        self.ready(at)?;
        self.validate_binding(claim)?;
        if !matches!(claim.owner, Owner::Physical(_)) {
            return Err(invalid("repeat requires a same-seat remap owner"));
        }
        self.at = at;
        let Some(held) = self.held.get(&claim.owner) else {
            return Ok(Progress::Complete);
        };
        self.stage(
            Mutation::Repeat(claim.owner.clone()),
            vec![RawEvent {
                kind: EV_KEY,
                code: held.keys[0],
                value: 2,
            }],
            None,
        )
    }

    /// Permanently retire this source incarnation and all older incarnations of its device.
    /// Exhausted source retirement capacity seals and drains the whole output.
    pub fn retire_source(&mut self, source: &Source, at: Duration) -> io::Result<Progress> {
        self.live(self.at)?;
        validate_source(source)?;
        if source.seat != self.source.seat {
            return Err(invalid("retired source belongs to another seat"));
        }
        if !self.retired_sources.contains_key(&source.device)
            && self.retired_sources.len() >= MAX_SOURCES
        {
            self.closing = true;
            self.at = self.at.max(at);
            return self.cleanup();
        }
        self.retired_sources
            .entry(source.device)
            .and_modify(|old| *old = (*old).max(source.generation))
            .or_insert(source.generation);
        self.at = self.at.max(at);
        self.cleanup()
    }

    /// Permanently retire remap and synthetic claims through the supplied configuration.
    pub fn retire_generation(&mut self, through: u64, at: Duration) -> io::Result<Progress> {
        self.live(self.at)?;
        if through == 0 {
            return Err(invalid("key generation must be nonzero"));
        }
        self.retired_generation = self.retired_generation.max(through);
        self.at = self.at.max(at);
        let through = self.retired_generation;
        self.receipts.retain(|generation, _| *generation > through);
        self.cleanup()
    }

    /// Seal the output, abandon unwritten presses and release all written or owned keys.
    pub fn retire(&mut self, at: Duration) -> io::Result<Progress> {
        self.live(self.at)?;
        self.closing = true;
        self.at = self.at.max(at);
        self.cleanup()
    }

    /// Flush at most one queued frame; ownership commits only after the full frame completes.
    pub fn flush(&mut self) -> io::Result<Progress> {
        self.live(self.at)?;
        if self.pending.is_none() {
            return self.cleanup();
        }
        match self.sink.as_mut().unwrap().flush() {
            Ok(Progress::Pending) => Ok(Progress::Pending),
            Err(error) => self.fail(error),
            Ok(Progress::Complete) => {
                let pending = self.pending.take().unwrap();
                if let Err(error) = self.commit(&pending.mutation) {
                    self.pending = Some(pending);
                    return self.fail(error);
                }
                if let Some(owner) = pending.release_after {
                    self.remove(vec![owner])?;
                }
                self.cleanup()
            }
        }
    }

    /// Confirmed local ownership only; this is not a consumer-neutral acknowledgement.
    pub fn down(&self, code: u16) -> bool {
        self.logical.down(&self.source.seat, code)
    }

    pub fn settled(&self) -> bool {
        !self.failed && self.pending.is_none()
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    pub fn retiring(&self) -> bool {
        self.closing
    }

    /// The possibly written frame after a terminal sink error; it must not be replayed.
    pub fn uncertain_frame(&self) -> Option<&Frame> {
        self.failed
            .then(|| self.pending.as_ref().map(|pending| &pending.frame))
            .flatten()
    }

    pub fn into_sink(self) -> Result<S, Box<Self>> {
        if self.failed
            || self.pending.is_some()
            || !self.held.is_empty()
            || !self.logical.counts.is_empty()
        {
            Err(Box::new(self))
        } else {
            Ok(self.sink.unwrap())
        }
    }

    fn validate_binding(&self, claim: &Claim) -> io::Result<()> {
        self.validate_claim(claim)?;
        if self
            .held
            .get(&claim.owner)
            .is_some_and(|held| held.claim != *claim)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "key operation belongs to a different claim",
            ));
        }
        Ok(())
    }

    fn validate_claim(&self, claim: &Claim) -> io::Result<()> {
        if claim.owner.seat() != self.source.seat || claim.configuration == 0 {
            return Err(invalid(
                "key owner requires this seat and a nonzero configuration",
            ));
        }
        if let Some(source) = &claim.origin {
            validate_source(source)?;
            if source.seat != self.source.seat {
                return Err(invalid("key origin belongs to another seat"));
            }
        }
        match &claim.owner {
            Owner::Physical(key) => {
                validate_source(&key.source)?;
                if !(1..768).contains(&key.code) || claim.origin.as_ref() != Some(&key.source) {
                    return Err(invalid("remap owner requires its exact physical origin"));
                }
            }
            Owner::Synthetic { generation, id, .. }
                if *generation != claim.configuration || *id == 0 =>
            {
                return Err(invalid(
                    "synthetic key owner requires nonzero generation and ID",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn cancelled(&self, claim: &Claim) -> bool {
        self.closing
            || claim.origin.as_ref().is_some_and(|source| {
                self.retired_sources
                    .get(&source.device)
                    .is_some_and(|generation| source.generation <= *generation)
            })
            || claim.configuration <= self.retired_generation
    }

    fn cleanup(&mut self) -> io::Result<Progress> {
        if let Some(pending) = &self.pending {
            let cancel = match &pending.mutation {
                Mutation::Add(held) => self.cancelled(&held.claim),
                Mutation::Repeat(owner) => self
                    .held
                    .get(owner)
                    .is_some_and(|held| self.cancelled(&held.claim)),
                Mutation::None | Mutation::Remove(_) => false,
            };
            if !cancel {
                return Ok(Progress::Pending);
            }
            let prefix = match self.sink.as_mut().unwrap().abort() {
                Ok(prefix) => prefix,
                Err(error) => return self.fail(error),
            };
            if !pending.frame.events.starts_with(&prefix) {
                return self.fail(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "key sink returned a different written prefix",
                ));
            }
            self.pending = None;
            let releases = prefix
                .iter()
                .rev()
                .filter(|event| event.kind == EV_KEY && event.value == 1)
                .map(|event| edge(event.code, false))
                .collect();
            if self.stage_sync(Mutation::None, releases, None, !prefix.is_empty())?
                == Progress::Pending
            {
                return Ok(Progress::Pending);
            }
        }
        let owners = self
            .held
            .values()
            .filter(|held| self.cancelled(&held.claim))
            .map(|held| held.claim.owner.clone())
            .collect();
        self.remove(owners)
    }

    fn remove(&mut self, owners: Vec<Owner>) -> io::Result<Progress> {
        let mut removed = BTreeMap::<u16, usize>::new();
        let mut events = Vec::new();
        for owner in &owners {
            if let Some(held) = self.held.get(owner) {
                for &code in held.keys.iter().rev() {
                    let count = removed.entry(code).or_default();
                    *count += 1;
                    if self.logical.counts[&(self.source.seat.clone(), code)] == *count {
                        events.push(edge(code, false));
                    }
                }
            }
        }
        self.stage(Mutation::Remove(owners), events, None)
    }

    fn stage(
        &mut self,
        mutation: Mutation,
        events: Vec<RawEvent>,
        release_after: Option<Owner>,
    ) -> io::Result<Progress> {
        self.stage_sync(mutation, events, release_after, false)
    }

    fn stage_sync(
        &mut self,
        mutation: Mutation,
        mut events: Vec<RawEvent>,
        release_after: Option<Owner>,
        force_sync: bool,
    ) -> io::Result<Progress> {
        if let Mutation::Add(held) = &mutation
            && matches!(held.claim.owner, Owner::Synthetic { generation, .. } if generation > self.retired_generation)
        {
            let Owner::Synthetic { id, .. } = held.claim.owner else {
                unreachable!()
            };
            self.receipts.insert(held.claim.configuration, id);
        }
        if events.is_empty() && !force_sync {
            if let Err(error) = self.commit(&mutation) {
                return self.fail(error);
            }
            return if let Some(owner) = release_after {
                self.remove(vec![owner])
            } else {
                Ok(Progress::Complete)
            };
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            return self.fail(invalid(
                "key frame sequence exhausted; consumer recovery required",
            ));
        };
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        self.pending = Some(Pending {
            frame: Frame {
                source: self.source.clone(),
                sequence,
                at: self.at,
                events,
            },
            mutation,
            release_after,
        });
        self.sequence = sequence;
        if let Err(error) = self
            .sink
            .as_mut()
            .unwrap()
            .enqueue(&self.pending.as_ref().unwrap().frame)
        {
            return self.fail(error);
        }
        Ok(Progress::Pending)
    }

    fn commit(&mut self, mutation: &Mutation) -> io::Result<()> {
        match mutation {
            Mutation::None | Mutation::Repeat(_) => {}
            Mutation::Add(held) => {
                for &code in &held.keys {
                    self.logical
                        .update(held.claim.owner.clone(), code, true)
                        .map_err(invalid)?;
                }
                self.held.insert(
                    held.claim.owner.clone(),
                    Held {
                        claim: held.claim.clone(),
                        keys: held.keys.clone(),
                    },
                );
            }
            Mutation::Remove(owners) => {
                for owner in owners {
                    self.logical.release_owner(owner);
                    self.held.remove(owner);
                }
            }
        }
        Ok(())
    }

    fn live(&self, at: Duration) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "key output failed; consumer recovery required",
            ));
        }
        if at < self.at {
            return Err(invalid("key output time moved backwards"));
        }
        Ok(())
    }

    fn ready(&self, at: Duration) -> io::Result<()> {
        self.live(at)?;
        if self.pending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "key frame is still pending",
            ));
        }
        Ok(())
    }

    fn fail<T>(&mut self, error: io::Error) -> io::Result<T> {
        self.failed = true;
        self.sink.take();
        Err(error)
    }
}

fn validate_source(source: &Source) -> io::Result<()> {
    if source.generation == 0
        || source.seat.is_empty()
        || source.seat.len() > 128
        || source.seat.chars().any(char::is_control)
    {
        return Err(invalid(
            "key output requires a bounded seat and nonzero source generation",
        ));
    }
    Ok(())
}

fn edge(code: u16, down: bool) -> RawEvent {
    RawEvent {
        kind: EV_KEY,
        code,
        value: i32::from(down),
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(target_os = "linux")]
impl Sink for crate::kernel::Virtual {
    fn source(&self) -> &Source {
        &self.descriptor().source
    }
    fn supports_key(&self, code: u16) -> bool {
        self.descriptor().class == crate::transport::Class::Keyboard
            && self
                .descriptor()
                .capabilities
                .get(&EV_KEY)
                .is_some_and(|keys| keys.contains(&code))
    }
    fn enqueue(&mut self, frame: &Frame) -> io::Result<()> {
        Self::enqueue(self, frame)
    }
    fn flush(&mut self) -> io::Result<Progress> {
        Self::flush(self).map(|value| match value {
            crate::kernel::Flush::Complete => Progress::Complete,
            crate::kernel::Flush::Pending => Progress::Pending,
        })
    }
    fn abort(&mut self) -> io::Result<Vec<RawEvent>> {
        self.cancel_pending()
    }
}

#[cfg(test)]
mod tests;
