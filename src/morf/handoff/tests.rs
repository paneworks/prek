use super::*;
use crate::{
    input::Source, ipc::Channel, morf::ownership::Ledger, protocol::Epoch, supervisor::Supervisor,
};
use std::{
    process::{Command, Stdio},
    thread,
    time::Instant,
};

fn fence(session: u64) -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session,
    }
}
fn state(phase: &str) -> native::HandoffState {
    // Explicit receiver-state model. This does not establish root/logind or
    // physical ownership acceptance; those require their separate live gates.
    serde_json::from_value(serde_json::json!({
        "version":1,"epoch":"a".repeat(32),"ui_generation":"2","handoff":{
            "phase":phase,"ownership_generation":"3","global_enabled":phase=="native",
            "native_neutral":true,"contacts":0,"contact_revision":"1","ui_ack":true,
            "driver":"native","observed_ns":"1000000000","scope":"global_phone_touch",
            "recognizer_ownership":false
        }
    }))
    .unwrap()
}
#[test]
fn quiescence_needs_exact_fresh_native_ack_and_never_means_owned() {
    let now = Duration::from_secs(1);
    for phase in [Phase::Quiescing, Phase::Quiesced] {
        assert_eq!(
            observed(phase, 3, 2, &state("quiesced"), now).unwrap(),
            Phase::Quiesced
        );
        assert_eq!(
            observed(phase, 3, 2, &state("quiescing"), now).unwrap(),
            Phase::Quiescing
        );
        for wrong in ["native", "owned", "releasing", "lost"] {
            assert!(observed(phase, 3, 2, &state(wrong), now).is_err());
        }
    }
    assert!(observed(Phase::Quiescing, 4, 2, &state("quiesced"), now).is_err());
    assert!(observed(Phase::Quiescing, 3, 3, &state("quiesced"), now).is_err());
    assert!(
        observed(
            Phase::Quiescing,
            3,
            2,
            &state("quiesced"),
            now + Duration::from_millis(250)
        )
        .is_err()
    );
    assert!(
        observed(
            Phase::Quiescing,
            3,
            2,
            &state("quiesced"),
            now - Duration::from_nanos(1)
        )
        .is_err()
    );
    for (field, value) in [
        ("ui_ack", serde_json::json!(false)),
        ("native_neutral", serde_json::json!(false)),
        ("contacts", serde_json::json!(1)),
        ("driver", serde_json::json!("lisgd")),
    ] {
        let mut value_state = state_value();
        value_state["handoff"][field] = value;
        let state = serde_json::from_value(value_state).unwrap();
        assert!(observed(Phase::Quiescing, 3, 2, &state, now).is_err());
    }
}
fn state_value() -> serde_json::Value {
    // HandoffState is deliberately not serializable into authority claims.
    serde_json::json!({"version":1,"epoch":"a".repeat(32),"ui_generation":"2","handoff":{
        "phase":"quiesced","ownership_generation":"3","global_enabled":false,
        "native_neutral":true,"contacts":0,"contact_revision":"1","ui_ack":true,
        "driver":"native","observed_ns":"1000000000","scope":"global_phone_touch",
        "recognizer_ownership":false}})
}
#[test]
fn queued_release_requires_a_native_restoration_observation() {
    let now = Duration::from_secs(1);
    assert_eq!(
        observed(Phase::ReleaseQueued, 3, 2, &state("releasing"), now).unwrap(),
        Phase::ReleaseQueued
    );
    assert_eq!(
        observed(Phase::ReleaseQueued, 3, 2, &state("native"), now).unwrap(),
        Phase::Restored
    );
    for phase in [
        Phase::Retired,
        Phase::Lost,
        Phase::ReleaseFailed,
        Phase::Restored,
    ] {
        assert!(observed(phase, 3, 2, &state("native"), now).is_err());
    }
    for remote in ["owned", "lost", "quiesced"] {
        assert!(observed(Phase::ReleaseQueued, 3, 2, &state(remote), now).is_err());
    }
}

#[test]
fn owned_observations_cannot_regress_or_cross_native_generations() {
    let now = Duration::from_secs(1);
    assert_eq!(
        observed(Phase::Owned, 3, 2, &state("owned"), now).unwrap(),
        Phase::Owned
    );
    for remote in ["native", "quiescing", "quiesced", "releasing", "lost"] {
        assert!(observed(Phase::Owned, 3, 2, &state(remote), now).is_err());
    }
    assert!(observed(Phase::Owned, 3, 3, &state("owned"), now).is_err());
    assert!(observed(Phase::Owned, 4, 2, &state("owned"), now).is_err());
    assert!(observed(Phase::Activating, 3, 2, &state("owned"), now).is_err());
}
#[test]
fn cleanup_cannot_reuse_or_retarget_the_retired_scope() {
    validate_cleanup(fence(1), fence(2)).unwrap();
    assert!(validate_cleanup(fence(1), fence(1)).is_err());
    assert!(validate_cleanup(fence(2), fence(1)).is_err());
    let mut foreign = fence(2);
    foreign.broker = Epoch([3, 4]);
    assert!(validate_cleanup(fence(1), foreign).is_err());
    foreign = fence(2);
    foreign.configuration = 0;
    assert!(validate_cleanup(fence(1), foreign).is_err());
}
#[test]
fn descriptor_closure_without_worker_exit_cannot_restore_recognition() {
    let (channel, _) = Channel::pair().unwrap();
    // Explicit identity model; no process-exit authority is manufactured.
    let worker: WorkerIdentity = serde_json::from_value(serde_json::json!({
        "pid":channel.peer().pid,"birth":1,"epoch":1
    }))
    .unwrap();
    let mut ledger = Ledger::new(worker, channel.peer()).unwrap();
    ledger.watch(fence(1), "seat0").unwrap();
    let source = Source {
        device: 1,
        generation: 1,
        seat: "seat0".into(),
    };
    ledger.prepared(fence(1), source.clone()).unwrap();
    ledger.retire_except(&[]);
    ledger
        .released(&channel, worker, fence(1), &source)
        .unwrap();
    let proof = ledger.take(fence(1), "seat0").unwrap();
    assert!(!proof.worker_exited());
    assert!(validate_release(fence(1), "seat0", worker, Phase::Retired, &proof).is_err());
}
#[test]
fn actual_reaped_worker_proof_is_scoped_and_release_is_not_repeatable() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--list")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (mut supervisor, channel) = Supervisor::spawn_controlled(command).unwrap();
    let worker = supervisor.worker_identity();
    let mut ledger = Ledger::new(worker, channel.peer()).unwrap();
    ledger.watch(fence(1), "seat0").unwrap();
    ledger.retire_except(&[]);
    assert!(ledger.take(fence(1), "seat0").is_none());
    let until = Instant::now() + Duration::from_secs(4);
    let report = loop {
        if let Some(report) = supervisor.check().unwrap() {
            break report;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    };
    ledger.exited(&report).unwrap();
    let proof = ledger.take(fence(1), "seat0").unwrap();
    for phase in [
        Phase::Quiescing,
        Phase::Quiesced,
        Phase::Activating,
        Phase::Owned,
        Phase::Retired,
        Phase::Lost,
    ] {
        validate_release(fence(1), "seat0", worker, phase, &proof).unwrap();
    }
    for phase in [Phase::ReleaseQueued, Phase::ReleaseFailed, Phase::Restored] {
        assert!(validate_release(fence(1), "seat0", worker, phase, &proof).is_err());
    }
    assert!(validate_release(fence(2), "seat0", worker, Phase::Retired, &proof).is_err());
    assert!(validate_release(fence(1), "seat1", worker, Phase::Retired, &proof).is_err());
    let other: WorkerIdentity = serde_json::from_value(serde_json::json!({
        "pid":worker.pid(),"birth":1,"epoch":u64::MAX
    }))
    .unwrap();
    assert!(validate_release(fence(1), "seat0", other, Phase::Retired, &proof).is_err());
}
