use super::*;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Call {
    Read(Client),
    Grab,
    Barrier,
    Release,
}
#[derive(Default)]
struct Script {
    witness: VecDeque<io::Result<Vec<InputEvent>>>,
    reader: VecDeque<io::Result<Vec<InputEvent>>>,
    late: Vec<InputEvent>,
    calls: Vec<Call>,
    grabbed: bool,
    fail_grab: bool,
    fail_barrier: bool,
    fail_release: bool,
}
struct Fake(Rc<RefCell<Script>>);
impl Port for Fake {
    fn read(&mut self, client: Client) -> io::Result<Vec<InputEvent>> {
        let mut script = self.0.borrow_mut();
        script.calls.push(Call::Read(client));
        match client {
            Client::Witness => script.witness.pop_front(),
            Client::Reader => script.reader.pop_front(),
        }
        .unwrap_or_else(|| Err(io::ErrorKind::WouldBlock.into()))
    }
    fn grab(&mut self) -> io::Result<()> {
        let mut script = self.0.borrow_mut();
        script.calls.push(Call::Grab);
        if script.fail_grab {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        script.grabbed = true;
        Ok(())
    }
    fn synchronize(&mut self) -> io::Result<()> {
        let mut script = self.0.borrow_mut();
        script.calls.push(Call::Barrier);
        if script.fail_barrier {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let late = std::mem::take(&mut script.late);
        if !late.is_empty() {
            script.witness.push_back(Ok(late.clone()));
            script.reader.push_back(Ok(late));
        }
        Ok(())
    }
    fn release(&mut self) -> io::Result<()> {
        let mut script = self.0.borrow_mut();
        script.calls.push(Call::Release);
        if script.fail_release {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        script.grabbed = false;
        Ok(())
    }
}
fn setup() -> (Protocol<Fake>, Rc<RefCell<Script>>) {
    let script = Rc::new(RefCell::new(Script::default()));
    (Protocol::new(Fake(script.clone())), script)
}
fn frame(value: i32) -> Vec<InputEvent> {
    vec![
        InputEvent::new(crate::input::EV_REL, 0, value),
        InputEvent::new(EV_SYN, SYN_REPORT, 0),
    ]
}
fn lost() -> Vec<InputEvent> {
    vec![
        InputEvent::new(EV_SYN, SYN_DROPPED, 0),
        InputEvent::new(EV_SYN, SYN_REPORT, 0),
    ]
}
fn grabbed() -> (Protocol<Fake>, Rc<RefCell<Script>>) {
    let (mut protocol, script) = setup();
    assert_eq!(protocol.prepare().unwrap(), Preparation::Armed);
    protocol.grab().unwrap();
    script.borrow_mut().calls.clear();
    (protocol, script)
}

#[test]
fn preparation_returns_original_events_instead_of_discarding_them() {
    let (mut protocol, script) = setup();
    script.borrow_mut().witness.push_back(Ok(frame(1)));
    script.borrow_mut().reader.push_back(Ok(frame(2)));
    assert_eq!(protocol.prepare().unwrap(), Preparation::Observe(frame(2)));
    assert_eq!(protocol.prepare().unwrap(), Preparation::Armed);
    assert!(!script.borrow().grabbed);
    assert_eq!(
        &script.borrow().calls[..],
        [
            Call::Read(Client::Witness),
            Call::Read(Client::Witness),
            Call::Read(Client::Reader),
            Call::Read(Client::Witness),
            Call::Read(Client::Reader)
        ]
    );
}

#[test]
fn empty_prefix_never_reads_or_discards_postgrab_input() {
    let (mut protocol, script) = grabbed();
    script.borrow_mut().reader.push_back(Ok(frame(8)));
    let cut = protocol.classify().unwrap();
    assert!(cut.observed.is_empty() && cut.retained.is_empty());
    assert_eq!(script.borrow().reader.len(), 1);
    assert_eq!(
        script.borrow().calls,
        [Call::Barrier, Call::Read(Client::Witness)]
    );
    assert_eq!(
        protocol.commit().unwrap(),
        Cut {
            observed: vec![],
            retained: vec![]
        }
    );
    drop(protocol);
    assert!(script.borrow().grabbed);
    assert!(!script.borrow().calls.contains(&Call::Release));
}

#[test]
fn equal_timestamps_and_repeated_identical_frames_are_matched_by_prefix_count() {
    let (mut protocol, script) = grabbed();
    let same = frame(4);
    let prefix: Vec<_> = same.iter().chain(&same).copied().collect();
    let reader: Vec<_> = prefix.iter().chain(&same).copied().collect();
    script.borrow_mut().witness.push_back(Ok(prefix.clone()));
    script.borrow_mut().reader.push_back(Ok(reader));
    assert_eq!(
        protocol.classify().unwrap(),
        &Cut {
            observed: prefix.clone(),
            retained: same.clone()
        }
    );
    assert_eq!(
        protocol.commit().unwrap(),
        Cut {
            observed: prefix,
            retained: same
        }
    );
}

#[test]
fn multi_read_prefix_retains_even_a_partial_following_frame_without_reordering() {
    let (mut protocol, script) = grabbed();
    let prefix = frame(3);
    let tail = vec![InputEvent::new(crate::input::EV_REL, 0, 17)];
    script
        .borrow_mut()
        .witness
        .extend([Ok(prefix[..1].to_vec()), Ok(prefix[1..].to_vec())]);
    script.borrow_mut().reader.extend([
        Ok(prefix[..1].to_vec()),
        Ok(prefix[1..].iter().chain(&tail).copied().collect()),
    ]);
    assert_eq!(
        protocol.classify().unwrap(),
        &Cut {
            observed: prefix,
            retained: tail
        }
    );
}

#[test]
fn in_flight_broadcast_is_completed_by_the_barrier_before_witness_freeze() {
    let (mut protocol, script) = grabbed();
    script.borrow_mut().late = frame(7);
    assert_eq!(protocol.classify().unwrap().observed, frame(7));
    assert_eq!(script.borrow().calls.first(), Some(&Call::Barrier));
    assert!(script.borrow().reader.is_empty());
}

#[test]
fn mismatch_and_missing_prefix_poison_the_protocol_and_release_only_its_grab() {
    for reader in [vec![], frame(9)] {
        let (mut protocol, script) = grabbed();
        script.borrow_mut().witness.push_back(Ok(frame(1)));
        script.borrow_mut().reader.push_back(Ok(reader));
        assert!(protocol.classify().is_err());
        assert!(protocol.commit().is_err());
        assert!(protocol.prepare().is_err());
        drop(protocol);
        assert!(!script.borrow().grabbed);
        assert_eq!(
            script
                .borrow()
                .calls
                .iter()
                .filter(|call| **call == Call::Release)
                .count(),
            1
        );
    }
}

#[test]
fn synchronization_loss_in_witness_prefix_or_retained_tail_is_never_a_cut() {
    for client in [Client::Witness, Client::Reader] {
        let (mut protocol, script) = grabbed();
        script
            .borrow_mut()
            .witness
            .push_back(Ok(if client == Client::Witness {
                lost()
            } else {
                frame(1)
            }));
        script
            .borrow_mut()
            .reader
            .push_back(Ok(frame(1).into_iter().chain(lost()).collect()));
        assert!(
            protocol
                .classify()
                .unwrap_err()
                .to_string()
                .contains("synchronization loss")
        );
        drop(protocol);
        assert!(!script.borrow().grabbed);
    }
}

#[test]
fn frozen_witness_must_end_at_a_report_boundary() {
    let (mut protocol, script) = grabbed();
    script
        .borrow_mut()
        .witness
        .push_back(Ok(vec![InputEvent::new(crate::input::EV_REL, 0, 1)]));
    assert!(
        protocol
            .classify()
            .unwrap_err()
            .to_string()
            .contains("inside a frame")
    );
    assert!(!script.borrow().calls.contains(&Call::Read(Client::Reader)));
}

#[test]
fn prepare_and_commit_phase_errors_cannot_create_an_owning_cut() {
    let (mut protocol, script) = setup();
    assert!(protocol.grab().is_err());
    assert!(protocol.prepare().is_err());
    drop(protocol);
    assert!(script.borrow().calls.is_empty());
    let (mut protocol, script) = setup();
    protocol.prepare().unwrap();
    assert!(protocol.commit().is_err());
    assert!(protocol.grab().is_err());
    drop(protocol);
    assert!(!script.borrow().calls.contains(&Call::Release));
}

#[test]
fn failed_grab_never_releases_a_foreign_owner() {
    let (mut protocol, script) = setup();
    protocol.prepare().unwrap();
    script.borrow_mut().fail_grab = true;
    assert!(protocol.grab().is_err());
    assert!(protocol.classify().is_err());
    drop(protocol);
    assert!(!script.borrow().calls.contains(&Call::Release));
}

#[test]
fn dropping_uncommitted_classification_or_error_releases_the_grab_once() {
    for fail in [false, true] {
        let (mut protocol, script) = grabbed();
        script.borrow_mut().fail_barrier = fail;
        assert_eq!(protocol.classify().is_err(), fail);
        drop(protocol);
        assert!(!script.borrow().grabbed);
        assert_eq!(
            script
                .borrow()
                .calls
                .iter()
                .filter(|call| **call == Call::Release)
                .count(),
            1
        );
    }
}

#[test]
fn interrupted_reads_retry_but_every_syscall_and_event_is_bounded() {
    let (mut protocol, script) = setup();
    script.borrow_mut().witness.extend([
        Err(io::ErrorKind::Interrupted.into()),
        Err(io::ErrorKind::WouldBlock.into()),
    ]);
    assert_eq!(protocol.prepare().unwrap(), Preparation::Armed);
    let (mut protocol, script) = setup();
    script
        .borrow_mut()
        .witness
        .extend((0..MAX_READS).map(|_| Err(io::ErrorKind::Interrupted.into())));
    assert!(protocol.prepare().is_err());
    assert!(!script.borrow().grabbed);
    let (mut protocol, script) = grabbed();
    script.borrow_mut().witness.push_back(Ok(vec![
        InputEvent::new(crate::input::EV_REL, 0, 1);
        MAX_EVENTS + 1
    ]));
    assert!(protocol.classify().is_err());
    drop(protocol);
    assert!(!script.borrow().grabbed);
}

#[test]
fn combined_prefix_and_reader_tail_cannot_escape_the_event_limit() {
    let (mut protocol, script) = grabbed();
    script.borrow_mut().witness.push_back(Ok(frame(1)));
    script.borrow_mut().reader.push_back(Ok(frame(1)
        .into_iter()
        .chain(vec![
            InputEvent::new(crate::input::EV_REL, 0, 2);
            MAX_EVENTS - 2
        ])
        .collect()));
    assert!(protocol.classify().is_err());
}

#[test]
fn elapsed_work_budget_and_nontransient_read_failures_abort_without_a_cut() {
    let mut budget = Budget::new();
    budget.deadline = Instant::now() - Duration::from_millis(1);
    assert!(budget.check().is_err());
    for client in [Client::Witness, Client::Reader] {
        let (mut protocol, script) = setup();
        {
            let mut script = script.borrow_mut();
            match client {
                Client::Witness => &mut script.witness,
                Client::Reader => &mut script.reader,
            }
            .push_back(Err(io::ErrorKind::PermissionDenied.into()));
        }
        assert!(protocol.prepare().is_err());
        assert!(protocol.grab().is_err());
    }
}

#[test]
fn failed_release_is_not_a_successful_descriptor_close_acknowledgement() {
    let (protocol, script) = grabbed();
    script.borrow_mut().fail_release = true;
    drop(protocol);
    assert!(script.borrow().grabbed);
    assert_eq!(script.borrow().calls, [Call::Release]);
}

fn recovered(error: io::Error) -> (io::Error, ReadJournal) {
    error
        .into_inner()
        .unwrap()
        .downcast::<ReadFailure>()
        .unwrap()
        .into_parts()
}

#[test]
fn failed_prefix_comparison_preserves_the_entire_original_read_including_its_tail() {
    let (mut protocol, script) = grabbed();
    let mut prefix = frame(1);
    prefix.extend(frame(2));
    let mut read = frame(1);
    read.extend(frame(9));
    read.extend(frame(8675309));
    script.borrow_mut().witness.push_back(Ok(prefix.clone()));
    script.borrow_mut().reader.push_back(Ok(read.clone()));
    let error = protocol.classify().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!format!("{error:?}").contains("8675309"));
    assert!(!error.to_string().contains("8675309"));
    let (cause, journal) = recovered(error);
    assert!(cause.to_string().contains("prefix differ"));
    assert_eq!(journal.original(), read);
    assert_eq!(journal.matched_prefix(), 2);
    assert_eq!(journal.witness_events(), prefix.len());
    assert!(journal.witness_frozen() && journal.grab_acquired());
    assert!(script.borrow().grabbed);
    assert!(protocol.commit().is_err());
    assert_eq!(journal.into_original(), read);
    drop(protocol);
    assert!(!script.borrow().grabbed);
}

#[test]
fn a_late_original_read_failure_does_not_discard_an_already_matched_partial_prefix() {
    for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::BrokenPipe] {
        let (mut protocol, script) = grabbed();
        let prefix = frame(3);
        script.borrow_mut().witness.push_back(Ok(prefix.clone()));
        script
            .borrow_mut()
            .reader
            .extend([Ok(prefix[..1].to_vec()), Err(kind.into())]);
        let (_, journal) = recovered(protocol.classify().unwrap_err());
        assert_eq!(journal.original(), &prefix[..1]);
        assert_eq!(journal.matched_prefix(), 1);
        assert_eq!(journal.witness_events(), 2);
        assert!(journal.witness_frozen() && journal.grab_acquired());
    }
}

#[test]
fn synchronization_loss_keeps_the_original_batch_without_promoting_it_to_a_cut() {
    let (mut protocol, script) = grabbed();
    let prefix = frame(4);
    let mut read = prefix.clone();
    read.extend(lost());
    read.extend(frame(5));
    script.borrow_mut().witness.push_back(Ok(prefix));
    script.borrow_mut().reader.push_back(Ok(read.clone()));
    let (_, journal) = recovered(protocol.classify().unwrap_err());
    assert_eq!(journal.original(), read);
    assert_eq!(journal.matched_prefix(), 0);
    assert!(journal.witness_frozen());
    assert!(protocol.commit().is_err());
}

#[test]
fn budget_exhaustion_after_a_successful_original_read_preserves_even_the_final_batch() {
    let (mut protocol, script) = grabbed();
    let mut read = frame(6);
    read.extend(vec![
        InputEvent::new(crate::input::EV_REL, 0, 8675309);
        MAX_EVENTS
    ]);
    script.borrow_mut().witness.push_back(Ok(frame(6)));
    script.borrow_mut().reader.push_back(Ok(read.clone()));
    let error = protocol.classify().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let (_, journal) = recovered(error);
    assert!(journal.original() == read.as_slice());
    assert_eq!(journal.matched_prefix(), 0);
    assert!(journal.witness_frozen() && journal.grab_acquired());
}

#[test]
fn preparation_budget_failure_preserves_only_the_original_observation_not_witness_history() {
    let (mut protocol, script) = setup();
    script.borrow_mut().witness.push_back(Ok(vec![
        InputEvent::new(crate::input::EV_REL, 0, 1);
        MAX_EVENTS - 1
    ]));
    script.borrow_mut().reader.push_back(Ok(frame(7)));
    let error = protocol.prepare().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let (_, journal) = recovered(error);
    assert_eq!(journal.original(), frame(7));
    assert_eq!(journal.witness_events(), 0);
    assert_eq!(journal.matched_prefix(), 0);
    assert!(!journal.witness_frozen() && !journal.grab_acquired());
    assert!(!script.borrow().grabbed);
}

#[test]
fn failure_after_a_successful_uncommitted_classification_preserves_exact_original_order() {
    let (mut protocol, script) = grabbed();
    let prefix = frame(8);
    let mut read = prefix.clone();
    read.extend(frame(9));
    script.borrow_mut().witness.push_back(Ok(prefix.clone()));
    script.borrow_mut().reader.push_back(Ok(read.clone()));
    protocol.classify().unwrap();
    let (_, journal) = recovered(protocol.classify().unwrap_err());
    assert_eq!(journal.original(), read);
    assert_eq!(journal.matched_prefix(), prefix.len());
    assert!(journal.witness_frozen() && journal.grab_acquired());
    assert!(protocol.commit().is_err());
}

#[test]
fn successful_observations_are_not_duplicated_by_a_later_failed_attempt() {
    let (mut protocol, script) = setup();
    script.borrow_mut().reader.push_back(Ok(frame(10)));
    assert_eq!(protocol.prepare().unwrap(), Preparation::Observe(frame(10)));
    assert_eq!(protocol.prepare().unwrap(), Preparation::Armed);
    script.borrow_mut().fail_grab = true;
    let (_, journal) = recovered(protocol.grab().unwrap_err());
    assert!(journal.original().is_empty());
    assert_eq!(journal.matched_prefix(), 0);
    assert!(!journal.witness_frozen() && !journal.grab_acquired());
}

#[test]
fn original_io_cause_retains_errno_and_the_first_read_can_be_recovered_without_logging_raw_input() {
    let (mut protocol, script) = grabbed();
    let prefix = frame(11);
    script.borrow_mut().witness.push_back(Ok(prefix.clone()));
    script.borrow_mut().reader.extend([
        Ok(prefix[..1].to_vec()),
        Err(io::Error::from_raw_os_error(libc::ENODEV)),
    ]);
    let error = protocol.classify().unwrap_err();
    let kind = error.kind();
    let failed = error
        .get_ref()
        .unwrap()
        .downcast_ref::<ReadFailure>()
        .unwrap();
    assert_eq!(failed.journal().original(), &prefix[..1]);
    assert!(!format!("{:?}", failed.journal()).contains("InputEvent"));
    let (cause, journal) = recovered(error);
    assert_eq!(cause.kind(), kind);
    assert_eq!(cause.raw_os_error(), Some(libc::ENODEV));
    assert_eq!(journal.into_original(), prefix[..1]);
}
