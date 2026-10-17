//! Descriptor-bound, coalesced keyboard LED feedback in the device-owning loop.
use super::{RawDevice, get_version, libc, reopen};
use crate::{
    input::{EV_SYN, Frame, RawEvent, SYN_REPORT},
    kernel::{Batch, Flush, Virtual},
    transport::Descriptor,
};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::OpenOptions,
    io::{self, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::fs::OpenOptionsExt,
    },
    time::Duration,
};

const INTERVAL: Duration = Duration::from_millis(250);
type Values = BTreeMap<u16, i32>;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct Flight {
    batch: Batch,
    values: Values,
    started: Duration,
}
struct Relay {
    codes: Vec<u16>,
    desired: Option<Values>,
    applied: Values,
    flight: Option<Flight>,
    active: bool,
    last: Duration,
    failed: bool,
}
impl Relay {
    fn check_values(&self, values: &Values) -> io::Result<()> {
        if values.keys().copied().collect::<Vec<_>>() != self.codes
            || values.values().any(|value| !matches!(value, 0 | 1))
        {
            return Err(invalid("LED snapshot differs from bound capabilities"));
        }
        Ok(())
    }
    fn advance(&mut self, at: Duration) -> io::Result<()> {
        if self.failed || at < self.last || at.checked_add(INTERVAL * 2).is_none() {
            self.failed = true;
            return Err(invalid("LED feedback is retired or its clock regressed"));
        }
        self.last = at;
        Ok(())
    }
    fn snapshot(&mut self, desired: Values, actual: Values, at: Duration) -> io::Result<()> {
        self.advance(at)?;
        let result = (|| {
            self.check_values(&desired)?;
            self.check_values(&actual)?;
            self.desired = Some(desired);
            if self.flight.is_none() {
                self.applied = actual;
            }
            Ok(())
        })();
        self.failed |= result.is_err();
        result
    }
    fn pending(&self) -> bool {
        !self.failed
            && self.active
            && (self.flight.is_some()
                || self
                    .desired
                    .as_ref()
                    .is_some_and(|desired| desired != &self.applied))
    }
    fn step(
        &mut self,
        writer: &mut impl Write,
        active: bool,
        at: Duration,
    ) -> io::Result<Option<Values>> {
        self.advance(at)?;
        let result = self.step_inner(writer, active, at);
        self.failed |= result.is_err();
        result
    }
    fn step_inner(
        &mut self,
        writer: &mut impl Write,
        active: bool,
        at: Duration,
    ) -> io::Result<Option<Values>> {
        if self.active && !active {
            return Err(invalid("active LED feedback must be retired, not paused"));
        }
        self.active = active;
        if !active {
            return Ok(None);
        }
        if self.flight.is_none() {
            let desired = self
                .desired
                .as_ref()
                .ok_or_else(|| invalid("LED target is not ready"))?;
            let values: Values = desired
                .iter()
                .filter(|(code, value)| self.applied.get(code) != Some(value))
                .map(|(&code, &value)| (code, value))
                .collect();
            if values.is_empty() {
                return Ok(None);
            }
            let events: Vec<_> = values
                .iter()
                .map(|(&code, &value)| RawEvent {
                    kind: 17,
                    code,
                    value,
                })
                .collect();
            self.flight = Some(Flight {
                batch: Batch::events(&events)?,
                values,
                started: at,
            });
        }
        let flight = self.flight.as_mut().unwrap();
        if at.saturating_sub(flight.started) >= INTERVAL {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "physical LED write exceeded 250ms",
            ));
        }
        if flight.batch.flush(writer)? == Flush::Pending {
            return Ok(None);
        }
        let values = self.flight.take().unwrap().values;
        self.applied.extend(values.clone());
        Ok(Some(values))
    }
    fn acknowledge(&mut self, written: &Values, actual: Values) -> io::Result<()> {
        let result = (|| {
            self.check_values(&actual)?;
            if written
                .iter()
                .any(|(code, value)| actual.get(code) != Some(value))
            {
                return Err(invalid(
                    "physical LED readback differs from completed write",
                ));
            }
            self.applied = actual;
            Ok(())
        })();
        self.failed |= result.is_err();
        result
    }
}

struct Writer<'a>(BorrowedFd<'a>);
impl Write for Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        nix::unistd::write(self.0, bytes).map_err(io::Error::from)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Owns independent source-write and virtual-query clients, never a grab clone.
/// Only declared LED values can be written; repeat/FF/raw-key injection is refused.
pub struct LedFeedback {
    source: RawDevice,
    target: Option<RawDevice>,
    descriptor: Descriptor,
    relay: Relay,
    sampled: Option<Duration>,
    next: Duration,
}
impl LedFeedback {
    pub fn new(original: &RawDevice, descriptor: &Descriptor) -> io::Result<Self> {
        descriptor.validate().map_err(invalid)?;
        let codes = descriptor
            .capabilities
            .get(&17)
            .cloned()
            .ok_or_else(|| invalid("LED bridge requires declared LED capabilities"))?;
        if descriptor.repeat.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "repeat feedback parity is not implemented",
            ));
        }
        let source = RawDevice::from_fd(reopen(original.as_fd(), true)?.into())?;
        let mut version = 0;
        // SAFETY: the original owned evdev client and an initialized c_int.
        unsafe { get_version(original.as_raw_fd(), &mut version) }.map_err(io::Error::from)?;
        if source.input_id() != original.input_id()
            || !identity_matches(&source, descriptor)
            || source
                .supported_leds()
                .map(|values| values.iter().map(|code| code.0).collect::<Vec<_>>())
                != Some(codes.clone())
        {
            return Err(invalid("source LED capabilities or connection differ"));
        }
        let applied = Self::values(&source, &codes)?;
        Ok(Self {
            source,
            target: None,
            descriptor: descriptor.clone(),
            relay: Relay {
                codes,
                desired: None,
                applied,
                flight: None,
                active: false,
                last: Duration::ZERO,
                failed: false,
            },
            sampled: None,
            next: Duration::ZERO,
        })
    }
    fn values(raw: &RawDevice, codes: &[u16]) -> io::Result<Values> {
        let state = raw.get_led_state()?;
        Ok(codes
            .iter()
            .map(|&code| (code, i32::from(state.contains(evdev::LedCode(code)))))
            .collect())
    }
    pub fn ready(&self, at: Duration) -> bool {
        !self.relay.failed
            && self.target.is_some()
            && self.relay.desired.is_some()
            && self
                .sampled
                .is_some_and(|sampled| sampled <= at && at - sampled <= INTERVAL * 2)
    }
    /// Stage pre-grab requests; only an active owning worker writes the source.
    pub fn service(&mut self, output: &mut Virtual, active: bool, at: Duration) -> io::Result<()> {
        let result = self.service_inner(output, active, at);
        self.relay.failed |= result.is_err();
        result
    }
    fn service_inner(
        &mut self,
        output: &mut Virtual,
        active: bool,
        at: Duration,
    ) -> io::Result<()> {
        self.relay.advance(at)?;
        if output.descriptor() != &self.descriptor {
            return Err(invalid("LED feedback target descriptor changed"));
        }
        let requests = output.feedback()?;
        if self.target.is_none() {
            let nodes = match output.event_nodes() {
                Err(error) if !active && error.kind() == io::ErrorKind::NotFound => {
                    self.next = at + INTERVAL;
                    return Ok(());
                }
                result => result?,
            };
            if nodes.is_empty() && !active {
                self.next = at + INTERVAL;
                return Ok(());
            }
            if nodes.len() != 1 {
                return Err(invalid("LED target must have exactly one event node"));
            }
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&nodes[0]);
            let file = match file {
                Err(error) if !active && error.kind() == io::ErrorKind::NotFound => {
                    self.next = at + INTERVAL;
                    return Ok(());
                }
                result => result?,
            };
            let target = RawDevice::from_fd(file.into())?;
            if target.physical_path() != Some(self.descriptor.marker.as_str())
                || !identity_matches(&target, &self.descriptor)
                || target.name() != Some(self.descriptor.name.as_str())
                || target
                    .supported_leds()
                    .map(|values| values.iter().map(|code| code.0).collect::<Vec<_>>())
                    != Some(self.relay.codes.clone())
            {
                return Err(invalid(
                    "LED target node differs from the owned virtual device",
                ));
            }
            self.target = Some(target);
        }
        if self.sampled.is_none() || at >= self.next || !requests.is_empty() {
            let desired = Self::values(self.target.as_ref().unwrap(), &self.relay.codes)?;
            let actual = Self::values(&self.source, &self.relay.codes)?;
            self.relay.snapshot(desired, actual, at)?;
            self.sampled = Some(at);
            self.next = at + INTERVAL;
        }
        if let Some(written) = self
            .relay
            .step(&mut Writer(self.source.as_fd()), active, at)?
        {
            let actual = Self::values(&self.source, &self.relay.codes)?;
            self.relay.acknowledge(&written, actual)?;
        }
        Ok(())
    }
    pub fn pending(&self) -> bool {
        self.relay.pending()
    }
    /// Remove reverse LED fields without changing source input or its report.
    pub fn prepare_forward<'a>(&mut self, frame: &'a Frame) -> io::Result<Cow<'a, Frame>> {
        let result =
            if self.relay.failed || !self.relay.active || frame.source != self.descriptor.source {
                Err(invalid(
                    "LED passthrough source is inactive, retired or mismatched",
                ))
            } else {
                remove_echo(frame, &self.relay.codes)
            };
        self.relay.failed |= result.is_err();
        result
    }
    pub fn deadline(&self) -> Duration {
        self.relay
            .flight
            .as_ref()
            .map_or(self.next, |flight| self.next.min(flight.started + INTERVAL))
    }
}
impl AsFd for LedFeedback {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.source.as_fd()
    }
}

fn identity_matches(raw: &RawDevice, descriptor: &Descriptor) -> bool {
    let id = raw.input_id();
    let expected = descriptor.identity;
    (id.bus_type().0, id.vendor(), id.product(), id.version())
        == (
            expected.bus,
            expected.vendor,
            expected.product,
            expected.version,
        )
}

fn remove_echo<'a>(frame: &'a Frame, codes: &[u16]) -> io::Result<Cow<'a, Frame>> {
    if frame
        .events
        .last()
        .is_none_or(|event| event.kind != EV_SYN || event.code != SYN_REPORT)
        || frame.events[..frame.events.len().saturating_sub(1)]
            .iter()
            .any(|event| event.kind == EV_SYN)
        || frame.events.iter().any(|event| {
            event.kind == 17 && (!codes.contains(&event.code) || !matches!(event.value, 0 | 1))
        })
    {
        return Err(invalid("source feedback fields or frame boundaries differ"));
    }
    if frame.events.iter().any(|event| event.kind == 17) {
        let mut filtered = frame.clone();
        filtered.events.retain(|event| event.kind != 17);
        Ok(Cow::Owned(filtered))
    } else {
        Ok(Cow::Borrowed(frame))
    }
}

#[cfg(test)]
mod tests;
