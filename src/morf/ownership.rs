//! Input-worker descriptor evidence for recognizer handoff. A removal notice,
//! socket loss or timeout cannot complete a release. This evidence alone does
//! not prove downstream neutrality or grant native Morf ownership.
use crate::{
    input::Source,
    ipc::{Channel, Credentials},
    protocol::Fence,
    supervisor::{Report, WorkerIdentity},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

const MAX_SCOPES: usize = 64;
const MAX_SOURCES: usize = 4096;
type Key = ([u64; 2], u64, u64, String);
fn key(fence: Fence, seat: &str) -> Key {
    (
        fence.broker.0,
        fence.configuration,
        fence.session,
        seat.to_owned(),
    )
}
fn valid_source(source: &Source) -> bool {
    source.generation > 0
        && !source.seat.is_empty()
        && source.seat.len() <= 256
        && !source.seat.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::Epoch, supervisor::Supervisor};
    use std::{
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    fn fence(session: u64) -> Fence {
        Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session,
        }
    }
    fn source(device: u64, generation: u64) -> Source {
        Source {
            device,
            generation,
            seat: "seat0".into(),
        }
    }
    fn modeled_worker(channel: &Channel, epoch: u64) -> WorkerIdentity {
        // Explicit identity model for ledger policy, not process-exit evidence.
        serde_json::from_value(
            serde_json::json!({"pid":channel.peer().pid,"birth":1,"epoch":epoch}),
        )
        .unwrap()
    }
    fn ledger() -> (Ledger, Channel, Channel) {
        let (channel, peer) = Channel::pair().unwrap();
        (
            Ledger::new(modeled_worker(&channel, 1), channel.peer()).unwrap(),
            channel,
            peer,
        )
    }

    #[test]
    fn release_needs_retirement_and_every_exact_source_generation() {
        let (mut ledger, channel, _peer) = ledger();
        let f = fence(1);
        let a = source(1, 1);
        let b = source(1, 2);
        ledger.watch(f, "seat0").unwrap();
        ledger.prepared(f, a.clone()).unwrap();
        ledger.prepared(f, b.clone()).unwrap();
        ledger.released(&channel, ledger.worker, f, &a).unwrap();
        assert!(ledger.take(f, "seat0").is_none());
        ledger.retire_except(&[]);
        assert!(ledger.take(f, "seat0").is_none());
        ledger
            .released(&channel, ledger.worker, fence(2), &b)
            .unwrap();
        ledger
            .released(&channel, ledger.worker, f, &source(1, 3))
            .unwrap();
        assert!(ledger.take(f, "seat0").is_none());
        ledger.released(&channel, ledger.worker, f, &b).unwrap();
        let proof = ledger.take(f, "seat0").unwrap();
        assert_eq!(proof.fence(), f);
        assert_eq!(proof.seat(), "seat0");
        assert_eq!(proof.worker(), ledger.worker);
        assert_eq!(proof.sources(), &BTreeSet::from([a, b]));
        assert!(!proof.worker_exited());
        assert!(ledger.take(f, "seat0").is_none());
    }

    #[test]
    fn foreign_worker_and_credentials_cannot_complete_release() {
        let (mut ledger, channel, _peer) = ledger();
        let f = fence(1);
        let s = source(1, 1);
        ledger.watch(f, "seat0").unwrap();
        ledger.prepared(f, s.clone()).unwrap();
        assert!(ledger.prepared_by(&channel, ledger.worker, f, &s));
        assert!(!ledger.prepared_by(&channel, modeled_worker(&channel, 2), f, &s));
        assert!(!ledger.prepared_by(&channel, ledger.worker, fence(2), &s));
        ledger.retire_except(&[]);
        assert!(!ledger.prepared_by(&channel, ledger.worker, f, &s));
        assert!(
            ledger
                .released(&channel, modeled_worker(&channel, 2), f, &s)
                .is_err()
        );
        let (mut foreign, _) = Channel::pair().unwrap();
        foreign.expect_worker(channel.peer().pid + 1);
        assert!(ledger.released(&foreign, ledger.worker, f, &s).is_err());
        assert!(ledger.take(f, "seat0").is_none());
        assert!(Ledger::new(ledger.worker, foreign.peer()).is_err());
    }

    #[test]
    fn closed_or_retired_scope_cannot_reopen_or_become_active_again() {
        let (mut ledger, channel, _peer) = ledger();
        let f = fence(1);
        let s = source(1, 1);
        ledger.watch(f, "seat0").unwrap();
        assert!(ledger.watch(f, "seat0").is_err());
        ledger.prepared(f, s.clone()).unwrap();
        ledger.released(&channel, ledger.worker, f, &s).unwrap();
        assert!(ledger.prepared(f, s.clone()).is_err());
        ledger.retire_except(&[]);
        ledger.retire_except(&[(f, "seat0".into())]);
        assert!(ledger.prepared(f, source(2, 1)).is_err());
        assert!(ledger.take(f, "seat0").is_some());
    }

    #[test]
    fn empty_unwatched_and_invalid_sources_do_not_prove_release() {
        let (mut ledger, channel, _peer) = ledger();
        let f = fence(1);
        ledger.prepared(f, source(1, 1)).unwrap();
        assert!(ledger.scopes.is_empty());
        ledger.watch(f, "seat0").unwrap();
        ledger.retire_except(&[]);
        ledger
            .released(&channel, ledger.worker, f, &source(1, 1))
            .unwrap();
        assert!(ledger.take(f, "seat0").is_none());
        assert!(ledger.prepared(f, source(1, 0)).is_err());
        assert!(ledger.watch(f, "bad\nseat").is_err());
    }

    #[test]
    fn pending_scopes_and_sources_are_bounded_without_eviction() {
        let (mut ledger, _, _) = ledger();
        for session in 1..=MAX_SCOPES as u64 {
            ledger.watch(fence(session), "seat0").unwrap();
        }
        assert!(ledger.watch(fence(MAX_SCOPES as u64 + 1), "seat0").is_err());
        for device in 0..MAX_SOURCES as u64 {
            ledger.prepared(fence(1), source(device, 1)).unwrap();
        }
        assert!(
            ledger
                .prepared(fence(2), source(MAX_SOURCES as u64, 1))
                .is_err()
        );
        assert_eq!(ledger.scopes.len(), MAX_SCOPES);
        assert_eq!(
            ledger.scopes[&key(fence(1), "seat0")].sources.len(),
            MAX_SOURCES
        );
    }

    fn child() -> (Supervisor, Channel) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--list")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Supervisor::spawn_controlled(command).unwrap()
    }
    fn reap(worker: &mut Supervisor) -> Report {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(report) = worker.check().unwrap() {
                return report;
            }
            assert!(Instant::now() < deadline, "test child was not reaped");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn only_actual_reaped_worker_completes_empty_and_pending_scopes() {
        let (mut worker, channel) = child();
        let (mut other, _) = child();
        let mut ledger = Ledger::new(worker.worker_identity(), channel.peer()).unwrap();
        ledger.watch(fence(1), "seat0").unwrap();
        ledger.watch(fence(2), "seat0").unwrap();
        ledger.prepared(fence(2), source(1, 1)).unwrap();
        ledger.retire_except(&[]);
        assert!(ledger.take(fence(1), "seat0").is_none());
        assert!(ledger.exited(&reap(&mut other)).is_err());
        assert!(ledger.take(fence(2), "seat0").is_none());
        ledger.exited(&reap(&mut worker)).unwrap();
        let empty = ledger.take(fence(1), "seat0").unwrap();
        assert!(empty.worker_exited());
        assert!(empty.sources().is_empty());
        let pending = ledger.take(fence(2), "seat0").unwrap();
        assert!(pending.worker_exited());
        assert_eq!(pending.sources().len(), 1);
        assert!(ledger.watch(fence(3), "seat0").is_err());
        assert!(ledger.prepared(fence(3), source(1, 1)).is_err());
    }
}

/// Created only by the broker's worker ledger after every registered source
/// closed, or after the supervisor actually reaped that exact worker lifetime.
/// It cannot be deserialized from a public client message.
#[derive(Debug)]
pub struct ReleasedInputs {
    fence: Fence,
    seat: String,
    worker: WorkerIdentity,
    sources: BTreeSet<Source>,
    exited: bool,
}
impl ReleasedInputs {
    pub fn fence(&self) -> Fence {
        self.fence
    }
    pub fn seat(&self) -> &str {
        &self.seat
    }
    pub fn worker(&self) -> WorkerIdentity {
        self.worker
    }
    pub fn sources(&self) -> &BTreeSet<Source> {
        &self.sources
    }
    pub fn worker_exited(&self) -> bool {
        self.exited
    }
}
struct Scope {
    fence: Fence,
    seat: String,
    sources: BTreeMap<Source, bool>,
    retired: bool,
}
pub(crate) struct Ledger {
    worker: WorkerIdentity,
    credentials: Credentials,
    scopes: BTreeMap<Key, Scope>,
    exited: bool,
}
impl Ledger {
    pub fn matches_worker(&self, channel: &Channel, worker: WorkerIdentity) -> bool {
        !self.exited && channel.peer() == self.credentials && worker == self.worker
    }
    pub fn prepared_by(
        &self,
        channel: &Channel,
        worker: WorkerIdentity,
        fence: Fence,
        source: &Source,
    ) -> bool {
        !self.exited
            && channel.peer() == self.credentials
            && worker == self.worker
            && self
                .scopes
                .get(&key(fence, &source.seat))
                .is_some_and(|scope| !scope.retired && scope.sources.get(source) == Some(&false))
    }
    pub fn worker(&self) -> WorkerIdentity {
        self.worker
    }
    pub fn watching(&self, fence: Fence, seat: &str) -> bool {
        self.scopes.contains_key(&key(fence, seat))
    }
    pub fn new(worker: WorkerIdentity, credentials: Credentials) -> io::Result<Self> {
        // Caller binds identity from its owned Supervisor, never a session packet.
        if !worker.valid() || worker.pid() != credentials.pid {
            return Err(io::Error::other(
                "input worker credential/identity mismatch",
            ));
        }
        Ok(Self {
            worker,
            credentials,
            scopes: BTreeMap::new(),
            exited: false,
        })
    }
    pub fn prepared(&mut self, fence: Fence, source: Source) -> io::Result<()> {
        fence.validate()?;
        if self.exited || !valid_source(&source) {
            return Err(io::Error::other("invalid input ownership source or worker"));
        }
        let k = key(fence, &source.seat);
        if !self.scopes.contains_key(&k) {
            return Ok(());
        }
        let count: usize = self.scopes.values().map(|scope| scope.sources.len()).sum();
        let scope = self.scopes.get_mut(&k).unwrap();
        if scope.retired || scope.sources.get(&source) == Some(&true) {
            return Err(io::Error::other(
                "retired input ownership cannot be prepared again",
            ));
        }
        if !scope.sources.contains_key(&source) && count >= MAX_SOURCES {
            return Err(io::Error::other("too many pending input sources"));
        }
        scope.sources.entry(source).or_insert(false);
        Ok(())
    }
    pub fn watch(&mut self, fence: Fence, seat: &str) -> io::Result<()> {
        fence.validate()?;
        if self.exited || seat.is_empty() || seat.len() > 256 || seat.chars().any(char::is_control)
        {
            return Err(io::Error::other("invalid ownership scope"));
        }
        let k = key(fence, seat);
        if self.scopes.contains_key(&k) {
            return Err(io::Error::other("ownership scope was already enrolled"));
        }
        if self.scopes.len() >= MAX_SCOPES {
            return Err(io::Error::other("too many pending ownership scopes"));
        }
        self.scopes.insert(
            k,
            Scope {
                fence,
                seat: seat.to_owned(),
                sources: BTreeMap::new(),
                retired: false,
            },
        );
        Ok(())
    }
    pub fn retire_except(&mut self, active: &[(Fence, String)]) {
        for scope in self.scopes.values_mut() {
            if !active
                .iter()
                .any(|(fence, seat)| *fence == scope.fence && *seat == scope.seat)
            {
                scope.retired = true;
            }
        }
    }
    pub fn released(
        &mut self,
        channel: &Channel,
        worker: WorkerIdentity,
        fence: Fence,
        source: &Source,
    ) -> io::Result<()> {
        if channel.peer() != self.credentials || worker != self.worker || !valid_source(source) {
            return Err(io::Error::other(
                "release did not come from the bound input worker",
            ));
        }
        fence.validate()?;
        // Unoffered preparation can close after context retirement; it never
        // received consumer readiness and supplies no release authority here.
        if let Some(scope) = self.scopes.get_mut(&key(fence, &source.seat))
            && let Some(closed) = scope.sources.get_mut(source)
        {
            *closed = true;
        }
        Ok(())
    }
    pub fn exited(&mut self, report: &Report) -> io::Result<()> {
        if report.worker_identity() != self.worker {
            return Err(io::Error::other(
                "release report belongs to another worker lifetime",
            ));
        }
        self.exited = true;
        for scope in self.scopes.values_mut() {
            scope.retired = true;
            for closed in scope.sources.values_mut() {
                *closed = true;
            }
        }
        Ok(())
    }
    pub fn take(&mut self, fence: Fence, seat: &str) -> Option<ReleasedInputs> {
        let k = key(fence, seat);
        let scope = self.scopes.get(&k)?;
        if !scope.retired
            || scope.sources.is_empty() && !self.exited
            || scope.sources.values().any(|closed| !*closed)
        {
            return None;
        }
        let scope = self.scopes.remove(&k)?;
        Some(ReleasedInputs {
            fence: scope.fence,
            seat: scope.seat,
            worker: self.worker,
            sources: scope.sources.into_keys().collect(),
            exited: self.exited,
        })
    }
}
