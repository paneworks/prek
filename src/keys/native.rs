//! Unwritten generated-keyboard preparation bound to an owned native publication.

use super::output::Output;
use crate::{
    input::Source,
    kernel::{Publication, Registry, Virtual},
    supervisor::Heartbeat,
    transport::{Consumer, ConsumerEvidence, Descriptor, Readiness, Ready},
};
use std::{collections::BTreeMap, io, time::Duration};

struct Admission {
    gate: Option<Readiness>,
    consumer_at: Option<Duration>,
    deadline: Duration,
}

impl Admission {
    fn new(descriptor: Descriptor, consumer: Consumer, at: Duration) -> io::Result<Self> {
        if !descriptor.is_generated_keyboard() {
            return Err(invalid(
                "generated keyboard preparation requires its own descriptor role",
            ));
        }
        if descriptor.is_isolated() != matches!(&consumer, Consumer::IsolatedReceiver { .. }) {
            return Err(invalid(
                "generated publication and consumer isolation must match",
            ));
        }
        let deadline = at
            .checked_add(Duration::from_secs(5))
            .ok_or_else(|| invalid("generated keyboard deadline overflow"))?;
        Ok(Self {
            gate: Some(Readiness::new(descriptor, consumer, at).map_err(invalid)?),
            consumer_at: None,
            deadline,
        })
    }

    fn observe(&mut self, evidence: &ConsumerEvidence, at: Duration) -> io::Result<()> {
        self.gate
            .as_mut()
            .ok_or_else(stopped)?
            .consumer(evidence, at)
            .map_err(invalid)?;
        self.consumer_at = Some(evidence.at);
        Ok(())
    }

    fn fresh(&self, at: Duration) -> bool {
        self.consumer_at
            .is_some_and(|when| at >= when && at - when <= Duration::from_millis(250))
    }

    fn poll(
        &mut self,
        descriptor: &Descriptor,
        neutral: bool,
        at: Duration,
    ) -> io::Result<Option<Ready>> {
        let gate = self.gate.as_mut().ok_or_else(stopped)?;
        gate.neutral(false, at).map_err(invalid)?;
        if !neutral || !self.fresh(at) {
            return Ok(None);
        }
        let gate = self.gate.as_mut().unwrap();
        gate.metadata(descriptor, at).map_err(invalid)?;
        gate.neutral(true, at).map_err(invalid)?;
        self.gate
            .take()
            .unwrap()
            .finish(at)
            .map(Some)
            .map_err(invalid)
    }
}

/// A worker-owned generated keyboard which cannot emit input before activation.
pub struct Prepared {
    descriptor: Descriptor,
    device: Option<Virtual>,
    admission: Admission,
}

impl Prepared {
    pub fn new(
        registry: &Registry,
        health: &Heartbeat,
        source: Source,
        consumer: Consumer,
        settings: BTreeMap<String, String>,
    ) -> io::Result<Self> {
        let at = crate::ipc::monotonic()?;
        let descriptor = Descriptor::generated_keyboard(source, settings).map_err(invalid)?;
        let publication = registry.prepare_generated_for_worker(descriptor, health)?;
        Self::create(publication, consumer, at)
    }

    fn create(publication: Publication, consumer: Consumer, at: Duration) -> io::Result<Self> {
        let descriptor = publication.descriptor().clone();
        let admission = Admission::new(descriptor.clone(), consumer, at)?;
        Ok(Self {
            descriptor,
            admission,
            device: Some(Virtual::create(publication)?),
        })
    }

    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    #[cfg(feature = "native-test-harness")]
    pub(crate) fn isolated_for_worker(
        registry: &Registry,
        health: &Heartbeat,
        source: Source,
        consumer: Consumer,
    ) -> io::Result<Self> {
        let descriptor =
            Descriptor::generated_keyboard(source, BTreeMap::new()).map_err(invalid)?;
        let publication = registry.prepare_generated_isolated_for_worker(descriptor, health)?;
        Self::create(publication, consumer, crate::ipc::monotonic()?)
    }

    #[cfg(feature = "native-test-harness")]
    pub(crate) fn fixture_nodes(&self) -> io::Result<Option<Vec<std::path::PathBuf>>> {
        let device = self.device.as_ref().ok_or_else(stopped)?;
        if !device.processed_metadata()? {
            return Ok(None);
        }
        device.event_nodes().map(Some)
    }

    pub fn deadline(&self) -> Duration {
        self.admission.deadline
    }

    /// Observe only evidence obtained through the caller's authenticated consumer path.
    pub fn observe(&mut self, evidence: &ConsumerEvidence) -> io::Result<()> {
        let result = crate::ipc::monotonic().and_then(|at| self.admission.observe(evidence, at));
        if result.is_err() {
            self.stop();
        }
        result
    }

    /// Return None while waiting; errors discard the unwritten native publication.
    pub fn activate(&mut self) -> io::Result<Option<Output<Virtual>>> {
        let result = self.activate_inner();
        if result.is_err() {
            self.stop();
        }
        result
    }

    fn activate_inner(&mut self) -> io::Result<Option<Output<Virtual>>> {
        let at = crate::ipc::monotonic()?;
        self.admission.poll(&self.descriptor, false, at)?;
        if !self.admission.fresh(at) {
            return Ok(None);
        }
        let device = self.device.as_ref().ok_or_else(stopped)?;
        let neutral = device.generated_neutral()?;
        let at = crate::ipc::monotonic()?;
        let Some(ready) = self.admission.poll(&self.descriptor, neutral, at)? else {
            return Ok(None);
        };
        if !ready.valid_at(at) || !ready.matches_generated(device.descriptor()) {
            return Err(invalid(
                "generated keyboard readiness differs from its owned publication",
            ));
        }
        Output::new(self.device.take().unwrap()).map(Some)
    }

    fn stop(&mut self) {
        self.device.take();
        self.admission.gate.take();
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn stopped() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "generated keyboard preparation is no longer live",
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_isolation;
