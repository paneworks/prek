use super::*;
use crate::keys::producer::{Operation as RequestOp, Request};

const MAX_PRODUCERS: usize = 64;
const LEASE: Duration = Duration::from_millis(250);
const PROVIDER_BYTES: usize = 128;
const PRESS_BYTES: usize = 256;
const WORK_BYTES: usize = 64;

pub(super) struct Producer {
    expires: Duration,
    epoch: u64,
    sequence: u64,
    last_press: u64,
    closed: bool,
    presses: BTreeMap<u64, Press>,
}
struct Press {
    key: u16,
    claim: Option<Claim>,
    ending: bool,
    cancelled: bool,
}
#[derive(Clone, Copy)]
pub(super) enum Kind {
    Press,
    Repeat,
    Release,
}
pub(super) struct Work {
    pub(super) producer: [u64; 2],
    pub(super) id: u64,
    pub(super) kind: Kind,
    pub(super) position: InputPosition,
    pub(super) inputs: BTreeMap<Source, u64>,
}

impl<S: Sink> Seat<S> {
    pub(crate) fn producer_request(
        &mut self,
        routing: &Routing,
        request: &Request,
        at: Duration,
    ) -> io::Result<bool> {
        self.live(at)?;
        let producer = request.producer.0;
        if request.fence != self.fence
            || request.source != *self.output.source()
            || producer == [0, 0]
            || request.sequence == 0
        {
            return Ok(false);
        }
        let close = matches!(request.operation, RequestOp::Close {});
        let scope = routing.software_key_scope(self.fence, at);
        if !close
            && (self.paused || self.retiring.is_some() || self.output.retiring() || scope.is_none())
        {
            return Ok(false);
        }
        if !close && self.check_position(Some(scope.unwrap().1)).is_err() {
            return Ok(false);
        }
        if let RequestOp::Open { expires } = request.operation {
            if request.sequence != 1
                || self.software.contains_key(&producer)
                || self.output.producer_retired(&producer)
                || self.software.len() >= MAX_PRODUCERS
                || !lease(expires, at)
                || expires > scope.unwrap().2
                || self.capacity(PROVIDER_BYTES, 0).is_err()
            {
                return Ok(false);
            }
            self.software.insert(
                producer,
                Producer {
                    expires,
                    epoch: scope.unwrap().0,
                    sequence: 1,
                    last_press: 0,
                    closed: false,
                    presses: BTreeMap::new(),
                },
            );
            self.bytes += PROVIDER_BYTES;
            self.at = at;
            return Ok(true);
        }
        let Some(entry) = self.software.get(&producer) else {
            return Ok(false);
        };
        if entry.closed
            || if close {
                request.sequence <= entry.sequence
            } else {
                entry.sequence.checked_add(1) != Some(request.sequence)
            }
            || !close
                && (at >= entry.expires || scope.is_none_or(|(epoch, _, _)| epoch != entry.epoch))
        {
            return Ok(false);
        }
        if close {
            self.close_producers(&[producer], at)?;
            self.prune();
            return Ok(true);
        }
        if let RequestOp::Renew { expires } = request.operation {
            if !lease(expires, at) || expires > scope.unwrap().2 {
                return Ok(false);
            }
            let entry = self.software.get_mut(&producer).unwrap();
            entry.sequence = request.sequence;
            entry.expires = expires;
            self.at = at;
            return Ok(true);
        }
        let inputs = routing.software_key_prefix(&self.output.source().seat);
        let bytes = WORK_BYTES
            + inputs
                .keys()
                .map(|source| source.seat.len() + 32)
                .sum::<usize>();
        let (id, kind) = match request.operation {
            RequestOp::Press { id, key } => {
                let count = self.bindings.len()
                    + self
                        .software
                        .values()
                        .map(|entry| entry.presses.len())
                        .sum::<usize>();
                if id == 0
                    || id <= entry.last_press
                    || !self.output.supports_key(key)
                    || count >= MAX_BINDINGS
                    || self.capacity(PRESS_BYTES + bytes, 1).is_err()
                {
                    return Ok(false);
                }
                let entry = self.software.get_mut(&producer).unwrap();
                entry.presses.insert(
                    id,
                    Press {
                        key,
                        claim: None,
                        ending: false,
                        cancelled: false,
                    },
                );
                entry.last_press = id;
                self.bytes += PRESS_BYTES;
                (id, Kind::Press)
            }
            RequestOp::Repeat { id } | RequestOp::Release { id } => {
                if !entry
                    .presses
                    .get(&id)
                    .is_some_and(|press| !press.ending && !press.cancelled)
                    || self.capacity(bytes, 1).is_err()
                {
                    return Ok(false);
                }
                let release = matches!(request.operation, RequestOp::Release { .. });
                if release {
                    self.software
                        .get_mut(&producer)
                        .unwrap()
                        .presses
                        .get_mut(&id)
                        .unwrap()
                        .ending = true;
                }
                (id, if release { Kind::Release } else { Kind::Repeat })
            }
            _ => unreachable!(),
        };
        self.software.get_mut(&producer).unwrap().sequence = request.sequence;
        self.queue(
            Operation::Software(Work {
                producer,
                id,
                kind,
                position: scope.unwrap().1,
                inputs,
            }),
            bytes,
            at,
        );
        Ok(true)
    }

    pub(super) fn execute_software(&mut self, work: Work, at: Duration) -> io::Result<()> {
        let Some(press) = self
            .software
            .get(&work.producer)
            .filter(|entry| !entry.closed)
            .and_then(|entry| entry.presses.get(&work.id))
            .filter(|press| !press.cancelled)
        else {
            return Ok(());
        };
        match work.kind {
            Kind::Press => {
                let key = press.key;
                let serial = self.next_serial()?;
                let claim = Claim {
                    owner: Owner::Software {
                        seat: self.output.source().seat.clone(),
                        producer: work.producer,
                        generation: self.fence.configuration,
                        id: serial,
                    },
                    origin: None,
                    configuration: self.fence.configuration,
                    serial,
                };
                self.output.hold(claim.clone(), &[key], at)?;
                self.software
                    .get_mut(&work.producer)
                    .unwrap()
                    .presses
                    .get_mut(&work.id)
                    .unwrap()
                    .claim = Some(claim);
            }
            Kind::Repeat => {
                if let Some(claim) = &press.claim {
                    self.output.repeat(claim, at)?;
                }
            }
            Kind::Release => {
                if let Some(claim) = &press.claim {
                    self.output.release(claim, at)?;
                }
                if self.output.settled() {
                    self.remove_software_press(work.producer, work.id);
                }
            }
        }
        if !self.output.settled() {
            self.flight = Some(Flight {
                position: Some(work.position),
                kind: FlightKind::Software {
                    producer: work.producer,
                    id: work.id,
                    release: matches!(work.kind, Kind::Release),
                },
                inputs: work.inputs,
                deadline: at.saturating_add(MAX_AGE),
                bytes: 0,
            });
        }
        Ok(())
    }

    fn close_producers(&mut self, producers: &[[u64; 2]], at: Duration) -> io::Result<()> {
        self.remove_pending(|operation| matches!(operation,Operation::Software(work) if producers.contains(&work.producer)));
        for id in producers {
            if let Some(entry) = self.software.get_mut(id) {
                entry.closed = true;
                for press in entry.presses.values_mut() {
                    press.cancelled = true;
                }
            }
        }
        self.output.retire_producers(producers, at)?;
        self.at = self.at.max(at);
        self.mark_draining(at);
        Ok(())
    }
    pub(super) fn expire_software(&mut self, routing: &Routing, at: Duration) -> io::Result<()> {
        let scope = routing.software_key_scope(self.fence, at);
        let expired = self
            .software
            .iter()
            .filter(|(_, entry)| {
                !entry.closed
                    && (at >= entry.expires
                        || scope.is_none_or(|(epoch, _, _)| epoch != entry.epoch))
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if !expired.is_empty() {
            self.close_producers(&expired, at)?;
        }
        Ok(())
    }
    pub(super) fn retire_software(&mut self, at: Duration) -> io::Result<()> {
        let producers = self.software.keys().copied().collect::<Vec<_>>();
        if !producers.is_empty() {
            self.close_producers(&producers, at)?;
        }
        Ok(())
    }
    pub(super) fn software_cleanup_pending(&self) -> bool {
        self.software
            .values()
            .any(|entry| entry.closed || entry.presses.values().any(|press| press.cancelled))
    }
    pub(super) fn software_binding_count(&self) -> usize {
        self.software
            .values()
            .map(|entry| entry.presses.len())
            .sum()
    }
    pub(super) fn software_deadline(&self) -> Option<Duration> {
        self.software
            .values()
            .filter(|entry| !entry.closed)
            .map(|entry| entry.expires)
            .min()
    }
    pub(super) fn remove_software_press(&mut self, producer: [u64; 2], id: u64) {
        if self
            .software
            .get_mut(&producer)
            .is_some_and(|entry| entry.presses.remove(&id).is_some())
        {
            self.bytes -= PRESS_BYTES;
        }
    }
    pub(super) fn prune_software(&mut self) {
        let mut removed = Vec::new();
        for (producer, entry) in &self.software {
            for (id, press) in &entry.presses {
                if press.cancelled
                    && press
                        .claim
                        .as_ref()
                        .is_none_or(|claim| !self.output.contains_claim(claim))
                {
                    removed.push((*producer, *id));
                }
            }
        }
        for (producer, id) in removed {
            self.remove_software_press(producer, id);
        }
        self.software.retain(|_, entry| {
            if entry.closed && entry.presses.is_empty() {
                self.bytes -= PROVIDER_BYTES;
                false
            } else {
                true
            }
        });
    }
}
fn lease(expires: Duration, at: Duration) -> bool {
    at < expires && expires <= at.saturating_add(LEASE)
}
