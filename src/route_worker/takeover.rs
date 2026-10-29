//! Physical takeover from a stock consumer after a neutral, input-free window.
use super::*;
use crate::{
    input::{ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS, EV_KEY, EV_SW},
    linux::handoff::{Cut, Handoff, Preparation, ReadFailure},
};
use evdev::InputEvent;

/// Input-free interval required before arming a takeover.
pub(super) const QUIET: Duration = Duration::from_millis(100);

/// Kernel steps of one takeover attempt. Releasing or dropping an
/// uncommitted attempt ungrabs the original.
pub(super) trait Cutover {
    fn prepare(&mut self) -> io::Result<Preparation>;
    fn grab(&mut self) -> io::Result<()>;
    fn classify(&mut self) -> io::Result<Cut>;
    fn commit(&mut self) -> io::Result<Cut>;
    fn release(&mut self);
}

pub(super) struct Kernel<'a> {
    handoff: Option<Handoff<'a>>,
}
impl<'a> Kernel<'a> {
    pub(super) fn new(raw: &'a mut RawDevice) -> io::Result<Self> {
        Ok(Self {
            handoff: Some(Handoff::new(raw)?),
        })
    }
    fn handoff(&mut self) -> io::Result<&mut Handoff<'a>> {
        self.handoff
            .as_mut()
            .ok_or_else(|| invalid("takeover attempt was released"))
    }
}
impl Cutover for Kernel<'_> {
    fn prepare(&mut self) -> io::Result<Preparation> {
        self.handoff()?.prepare()
    }
    fn grab(&mut self) -> io::Result<()> {
        self.handoff()?.grab()
    }
    fn classify(&mut self) -> io::Result<Cut> {
        let cut = self.handoff()?.classify()?;
        Ok(Cut {
            observed: cut.observed.clone(),
            retained: cut.retained.clone(),
        })
    }
    fn commit(&mut self) -> io::Result<Cut> {
        self.handoff
            .take()
            .ok_or_else(|| invalid("takeover attempt was released"))?
            .commit()
    }
    fn release(&mut self) {
        self.handoff = None;
    }
}

/// A takeover committed at `at`, attached when its prefix is delivered.
pub(super) struct Committed {
    device: Device,
    ready: crate::transport::Ready,
    at: Duration,
}

/// Whether `observed`, applied from a neutral state, leaves no key, switch or
/// contact active.
pub(super) fn balanced(observed: &[InputEvent]) -> bool {
    let mut keys = BTreeSet::new();
    let mut switches = BTreeSet::new();
    let mut contacts = BTreeSet::new();
    let mut slot = -1;
    for event in observed {
        let (kind, code, value) = (event.event_type().0, event.code(), event.value());
        match kind {
            EV_KEY if value == 0 => {
                keys.remove(&code);
            }
            EV_KEY if value == 1 => {
                keys.insert(code);
            }
            EV_SW if value == 0 => {
                switches.remove(&code);
            }
            EV_SW => {
                switches.insert(code);
            }
            EV_ABS if code == ABS_MT_SLOT => slot = value,
            EV_ABS if code == ABS_MT_TRACKING_ID && value < 0 => {
                contacts.remove(&slot);
            }
            EV_ABS if code == ABS_MT_TRACKING_ID => {
                contacts.insert(slot);
            }
            _ => {}
        }
    }
    keys.is_empty() && switches.is_empty() && contacts.is_empty()
}

impl Stream {
    /// Runs one takeover attempt for a ready, neutral and idle source. Every
    /// original event the attempt consumed is returned through the disposition.
    pub(super) fn take_over(
        &mut self,
        device: &Device,
        cutover: &mut impl Cutover,
    ) -> io::Result<ReaderDisposition> {
        match cutover.prepare() {
            Ok(Preparation::Observe(events)) => return Ok(ReaderDisposition::Observe(events)),
            Ok(Preparation::Armed) => {}
            Err(error) => return Ok(self.abandon_takeover(device, error)),
        }
        let at = monotonic()?;
        if let Err(error) = cutover.grab() {
            return Ok(self.abandon_takeover(device, error));
        }
        let cut = match cutover.classify() {
            Ok(cut) => cut,
            Err(error) => {
                cutover.release();
                return Ok(self.abandon_takeover(device, error));
            }
        };
        if !balanced(&cut.observed) {
            cutover.release();
            let mut events = cut.observed;
            events.extend(cut.retained);
            return Ok(ReaderDisposition::Observe(events));
        }
        let ready = self
            .outputs
            .get_mut(&device.source)
            .and_then(|output| output.readiness.take())
            .ok_or_else(|| invalid("takeover has no readiness gate"))
            .and_then(|mut gate| {
                gate.neutral(true, at).map_err(invalid)?;
                gate.finish(at).map_err(invalid)
            });
        let ready = match ready {
            Ok(ready) => ready,
            Err(error) => {
                cutover.release();
                let mut events = cut.observed;
                events.extend(cut.retained);
                self.refuse_output(device, &error);
                return Ok(ReaderDisposition::Observe(events));
            }
        };
        let cut = cutover.commit()?;
        self.committed.insert(
            device.source.clone(),
            Committed {
                device: device.clone(),
                ready,
                at,
            },
        );
        Ok(ReaderDisposition::Cutover(cut))
    }
    /// Attaches a committed takeover once its observed prefix is delivered.
    pub(super) fn attach_takeover(&mut self, source: &Source) -> io::Result<()> {
        let Committed { device, ready, at } = self
            .committed
            .remove(source)
            .ok_or_else(|| invalid("reader cutover has no committed takeover"))?;
        // Routing starts from the axis and slot coordinates the observed
        // stream left at the cut.
        let absolute = self.baseline.absolute_state(source).cloned();
        let seat = self
            .seats
            .get_mut(&source.seat)
            .ok_or_else(|| invalid("takeover seat is gone"))?;
        eprintln!(
            "routing worker: {} taken over at a neutral idle cut",
            device.name
        );
        let name = device.name.clone();
        let mapping = seat.subscription.touch_mapping(&self.config, &device);
        if let Some(mapping) = &mapping {
            self.mapped.insert(source.clone(), mapping::target(mapping));
        }
        seat.routing
            .attach_with_state(device, &ready, mapping, absolute, at)?;
        self.grabbed.insert(source.clone());
        self.settle_forwarding();
        self.forwarding
            .insert(source.clone(), latency::Forwarding::new(name, at));
        Ok(())
    }
    /// Refuses a failed attempt and returns any original events it consumed.
    fn abandon_takeover(&mut self, device: &Device, error: io::Error) -> ReaderDisposition {
        let events = error
            .get_ref()
            .and_then(|cause| cause.downcast_ref::<ReadFailure>())
            .map(|failure| failure.journal().original().to_vec())
            .unwrap_or_default();
        self.refuse_output(device, &error);
        if events.is_empty() {
            ReaderDisposition::Keep
        } else {
            ReaderDisposition::Observe(events)
        }
    }
    /// Leaves a source with its original consumer and destroys its mirror.
    pub(super) fn refuse_output(&mut self, device: &Device, error: &dyn std::fmt::Display) {
        eprintln!(
            "routing worker: {} stays with its original consumer: {error}",
            device.name
        );
        self.outputs.remove(&device.source);
        self.refused.insert(device.source.clone());
    }
}

#[cfg(test)]
mod tests;
