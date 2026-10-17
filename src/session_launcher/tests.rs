use super::*;

#[test]
fn observed_sleep_boundary_restarts_outage_budget_without_renewing_launch_rate() {
    let mut budget = Budget::default();
    budget.attempt(Duration::ZERO).unwrap();
    budget.outage = None;
    budget.attempt(WINDOW + WINDOW).unwrap();
    for _ in 1..16 {
        budget.outage = None;
        budget.attempt(WINDOW + WINDOW).unwrap();
    }
    budget.outage = None;
    assert!(budget.attempt(WINDOW + WINDOW).is_err());
}

#[test]
fn budgets_bound_outages_and_ready_crash_loops_independently() {
    let mut budget = Budget::default();
    budget.attempt(Duration::ZERO).unwrap();
    assert!(budget.attempt(WINDOW).is_err());
    budget.accepted();
    budget.attempt(WINDOW).unwrap();
    let mut budget = Budget::default();
    for _ in 0..16 {
        budget.attempt(Duration::ZERO).unwrap();
        budget.accepted();
    }
    assert!(budget.attempt(Duration::ZERO).is_err());
    budget.attempt(WINDOW).unwrap();
}

#[test]
fn only_explicit_transport_or_retired_clock_errors_are_retryable() {
    for kind in [
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::InvalidData,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::NotFound,
        io::ErrorKind::TimedOut,
    ] {
        assert_eq!(exit_code(&classify(kind.into())), None);
    }
    let (mut channel, peer) = crate::ipc::Channel::pair().unwrap();
    drop(peer);
    let error = channel.receive::<bool>().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert!(crate::ipc::is_transport_loss(&error));
    assert_eq!(exit_code(&classify(error)), Some(RETRY_EXIT));
    let (mut channel, peer) = crate::ipc::Channel::pair().unwrap();
    drop(peer);
    let error = channel
        .send(&true, crate::ipc::monotonic().unwrap())
        .unwrap_err();
    assert!(crate::ipc::is_transport_loss(&error));
    assert_eq!(exit_code(&classify(error)), Some(RETRY_EXIT));
    assert_eq!(
        exit_code(&classify(retry(io::ErrorKind::NotFound.into()))),
        Some(RETRY_EXIT)
    );
}
