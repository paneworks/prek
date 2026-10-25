//! Bounded whole subscription updates on the authenticated private worker link.
//! Scope headers can revoke old rights; only committed bodies can activate them.
use crate::{
    configuration::{CHUNK_BYTES, Command},
    context::{Binding, State},
    ipc::Channel,
    protocol::{self, Capability, Fence},
    transport::Consumer,
    worker::{Subscription, Subscriptions},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io, time::Duration};

pub const MAX_BYTES: usize = crate::service::MAX_CLIENTS * crate::context_transfer::MAX_BYTES;
pub const TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextScope {
    binding: Binding,
    generation: u64,
    state: State,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub seat: String,
    pub fence: Fence,
    pub capabilities: BTreeSet<Capability>,
    consumer: Option<Consumer>,
    context: Option<ContextScope>,
}
impl Scope {
    pub fn from_subscription(subscription: &Subscription) -> Self {
        Self {
            seat: subscription.seat.clone(),
            fence: subscription.fence,
            capabilities: subscription.capabilities.clone(),
            consumer: subscription.consumer.clone(),
            context: subscription.context.as_ref().map(|context| ContextScope {
                binding: context.binding.clone(),
                generation: context.snapshot.generation,
                state: context.snapshot.state,
            }),
        }
    }
    pub fn retains(&self, old: &Subscription) -> bool {
        let old = Self::from_subscription(old);
        self.seat == old.seat
            && self.fence == old.fence
            && self.consumer == old.consumer
            && self.context == old.context
    }
    pub fn validate_transition(&self, old: &Subscription) -> io::Result<()> {
        if self.seat != old.seat || self.fence != old.fence {
            return Ok(());
        }
        match (&self.context, &old.context) {
            (None, None) => Ok(()),
            (Some(new), Some(old)) if new.binding == old.binding => Ok(()),
            _ => Err(protocol::invalid(
                "subscription binding transition requires a fresh fence",
            )),
        }
    }
    pub fn identity(&self) -> Subscription {
        Subscription {
            seat: self.seat.clone(),
            fence: self.fence,
            context: None,
            capabilities: BTreeSet::new(),
            consumer: None,
        }
    }
}

fn scopes(seats: &[Subscription]) -> Vec<Scope> {
    let mut scopes: Vec<_> = seats.iter().map(Scope::from_subscription).collect();
    scopes.sort_by(|a, b| a.seat.cmp(&b.seat));
    scopes
}
pub fn validate_scopes(scopes: &[Scope]) -> io::Result<()> {
    if scopes.len() > crate::service::MAX_CLIENTS {
        return Err(protocol::invalid("too many private subscription scopes"));
    }
    if scopes.windows(2).any(|pair| pair[0].seat >= pair[1].seat) {
        return Err(protocol::invalid(
            "private subscription scopes are not canonical",
        ));
    }
    let mut seats = BTreeSet::new();
    let mut epoch = None;
    let mut configuration = None;
    for scope in scopes {
        scope.fence.validate()?;
        if scope.seat.is_empty()
            || scope.seat.len() > 256
            || scope.seat.chars().any(char::is_control)
            || !seats.insert(&scope.seat)
            || epoch.is_some_and(|old| old != scope.fence.broker)
            || configuration.is_some_and(|old| old != scope.fence.configuration)
        {
            return Err(protocol::invalid("invalid private subscription scope"));
        }
        epoch = Some(scope.fence.broker);
        configuration = Some(scope.fence.configuration);
        if let Some(context) = &scope.context {
            let binding = &context.binding;
            if context.generation != scope.fence.session
                || binding.seat != scope.seat
                || binding.uid == 0
                || binding.compositor_pid == 0
                || binding.compositor_start == 0
                || [&binding.seat, &binding.session].into_iter().any(|name| {
                    name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control)
                })
            {
                return Err(protocol::invalid(
                    "private subscription context differs from scope",
                ));
            }
        }
    }
    Ok(())
}

fn encode(update: &Subscriptions) -> io::Result<Vec<u8>> {
    struct Buffer(Vec<u8>);
    impl io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
                return Err(protocol::invalid(
                    "private subscriptions exceed transfer bound",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut out = Buffer(Vec::new());
    serde_json::to_writer(&mut out, update).map_err(io::Error::other)?;
    Ok(out.0)
}
struct Transfer {
    revision: u64,
    scopes: Vec<Scope>,
    data: Vec<u8>,
    offset: usize,
    begun: bool,
    expires: Duration,
}
impl Transfer {
    fn new(update: Subscriptions, now: Duration) -> io::Result<Self> {
        if update.revision == 0 {
            return Err(protocol::invalid("private subscription revision is zero"));
        }
        let scopes = scopes(&update.seats);
        validate_scopes(&scopes)?;
        let data = encode(&update)?;
        let header = Command::SubscriptionsBegin {
            revision: update.revision,
            bytes: data.len(),
            scopes: scopes.clone(),
        };
        protocol::encode(&protocol::Packet {
            version: protocol::VERSION,
            sequence: u64::MAX,
            at: Duration::MAX,
            message: header,
        })?;
        Ok(Self {
            revision: update.revision,
            scopes,
            data,
            offset: 0,
            begun: false,
            expires: now
                .checked_add(TIMEOUT)
                .ok_or_else(|| protocol::invalid("private subscription clock exhausted"))?,
        })
    }
}

pub fn fits(seats: &[Subscription]) -> bool {
    #[derive(Serialize)]
    struct Body<'a> {
        revision: u64,
        seats: &'a [Subscription],
    }
    struct Size(usize);
    impl io::Write for Size {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_BYTES.saturating_sub(self.0) {
                return Err(protocol::invalid(
                    "private subscriptions exceed transfer bound",
                ));
            }
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let scopes = scopes(seats);
    if validate_scopes(&scopes).is_err() {
        return false;
    }
    let mut bytes = Size(0);
    if serde_json::to_writer(
        &mut bytes,
        &Body {
            revision: u64::MAX,
            seats,
        },
    )
    .is_err()
    {
        return false;
    }
    protocol::encode(&protocol::Packet {
        version: protocol::VERSION,
        sequence: u64::MAX,
        at: Duration::MAX,
        message: Command::SubscriptionsBegin {
            revision: u64::MAX,
            bytes: bytes.0,
            scopes,
        },
    })
    .is_ok()
}
#[derive(Default)]
pub struct Sender {
    revision: u64,
    active: Option<Transfer>,
    latest: Option<Transfer>,
}
impl Sender {
    pub fn replace(&mut self, update: Subscriptions, now: Duration) -> io::Result<()> {
        if update.revision <= self.revision {
            return Err(protocol::invalid(
                "private subscription sender revision regressed",
            ));
        }
        let next = Transfer::new(update, now)?;
        self.revision = next.revision;
        if self
            .active
            .as_ref()
            .is_none_or(|old| !old.begun || old.scopes != next.scopes)
        {
            self.active = Some(next);
            self.latest = None;
        } else {
            self.latest = Some(next)
        }
        Ok(())
    }
    pub fn pending(&self) -> bool {
        self.active.is_some() || self.latest.is_some()
    }
    pub fn clear(&mut self) {
        self.active = None;
        self.latest = None;
    }
    pub fn send(&mut self, channel: &mut Channel, now: Duration) -> io::Result<()> {
        for _ in 0..4 {
            if self
                .active
                .as_ref()
                .is_some_and(|active| now >= active.expires)
            {
                self.active = None;
            }
            if self.active.is_none() {
                self.active = self.latest.take();
                if let Some(active) = &mut self.active {
                    active.expires = now
                        .checked_add(TIMEOUT)
                        .ok_or_else(|| protocol::invalid("private subscription clock exhausted"))?;
                }
            }
            let Some(active) = &mut self.active else {
                break;
            };
            let command = if !active.begun {
                Command::SubscriptionsBegin {
                    revision: active.revision,
                    bytes: active.data.len(),
                    scopes: active.scopes.clone(),
                }
            } else if active.offset < active.data.len() {
                Command::SubscriptionsChunk {
                    revision: active.revision,
                    offset: active.offset,
                    data: active.data
                        [active.offset..(active.offset + CHUNK_BYTES).min(active.data.len())]
                        .to_vec(),
                }
            } else {
                Command::SubscriptionsCommit {
                    revision: active.revision,
                }
            };
            if !channel.send(&command, now)? {
                break;
            }
            match command {
                Command::SubscriptionsBegin { .. } => active.begun = true,
                Command::SubscriptionsChunk { data, .. } => active.offset += data.len(),
                Command::SubscriptionsCommit { .. } => self.active = None,
                _ => unreachable!(),
            }
        }
        Ok(())
    }
}

struct Pending {
    revision: u64,
    scopes: Vec<Scope>,
    bytes: usize,
    data: Vec<u8>,
    expires: Duration,
}
#[derive(Default)]
pub struct Assembly {
    revision: u64,
    pending: Option<Pending>,
}
pub enum Update {
    Scope(Vec<Scope>),
    Complete(Subscriptions),
}
impl Assembly {
    pub fn deadline(&self) -> Option<Duration> {
        self.pending.as_ref().map(|pending| pending.expires)
    }
    pub fn cancel(&mut self) {
        self.pending = None;
    }
    pub fn retire(&mut self, revision: u64) -> io::Result<()> {
        if revision == 0 {
            return Err(protocol::invalid("private subscription revision is zero"));
        }
        if revision > self.revision {
            self.revision = revision;
            self.pending = None;
        }
        Ok(())
    }
    pub fn direct(&mut self, revision: u64) -> io::Result<()> {
        if revision == 0 || revision <= self.revision {
            return Err(protocol::invalid(
                "private subscription update revision regressed",
            ));
        }
        self.revision = revision;
        self.pending = None;
        Ok(())
    }
    pub fn expire(&mut self, now: Duration) -> Option<Vec<Scope>> {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            return self.pending.take().map(|pending| pending.scopes);
        }
        None
    }
    pub fn accept(&mut self, command: Command, now: Duration) -> io::Result<Option<Update>> {
        match command {
            Command::SubscriptionsBegin {
                revision,
                bytes,
                scopes,
            } => {
                if revision == 0 || revision <= self.revision || bytes == 0 || bytes > MAX_BYTES {
                    return Err(protocol::invalid("invalid private subscription header"));
                }
                validate_scopes(&scopes)?;
                let expires = now
                    .checked_add(TIMEOUT)
                    .ok_or_else(|| protocol::invalid("private subscription clock exhausted"))?;
                self.revision = revision;
                self.pending = Some(Pending {
                    revision,
                    scopes: scopes.clone(),
                    bytes,
                    data: Vec::new(),
                    expires,
                });
                Ok(Some(Update::Scope(scopes)))
            }
            Command::SubscriptionsChunk {
                revision,
                offset,
                data,
            } => {
                if revision == 0 || revision > self.revision {
                    return Err(protocol::invalid("private subscription chunk lacks header"));
                }
                let Some(pending) = &mut self.pending else {
                    return Ok(None);
                };
                if revision < pending.revision {
                    return Ok(None);
                }
                if now >= pending.expires
                    || offset != pending.data.len()
                    || offset >= pending.bytes
                    || data.len() != CHUNK_BYTES.min(pending.bytes - offset)
                {
                    return Err(protocol::invalid(
                        "invalid or expired private subscription chunk",
                    ));
                }
                pending.data.extend_from_slice(&data);
                Ok(None)
            }
            Command::SubscriptionsCommit { revision } => {
                if revision == 0 || revision > self.revision {
                    return Err(protocol::invalid(
                        "private subscription commit lacks header",
                    ));
                }
                let Some(pending) = self.pending.as_ref() else {
                    return Ok(None);
                };
                if revision < pending.revision {
                    return Ok(None);
                }
                let pending = self.pending.take().unwrap();
                if now >= pending.expires || pending.data.len() != pending.bytes {
                    return Err(protocol::invalid(
                        "private subscription commit is incomplete or expired",
                    ));
                }
                let update: Subscriptions =
                    serde_json::from_slice(&pending.data).map_err(io::Error::other)?;
                if update.revision != revision || scopes(&update.seats) != pending.scopes {
                    return Err(protocol::invalid(
                        "private subscription body differs from authenticated header",
                    ));
                }
                Ok(Some(Update::Complete(update)))
            }
            _ => Err(protocol::invalid(
                "not a private subscription transfer command",
            )),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
