use super::*;

fn at(ms: u64) -> Duration {
    Duration::from_millis(ms)
}
fn viewport(direction: Direction, offset: f64) -> Viewport {
    Viewport::new(direction, 800., 2400., offset).unwrap()
}
fn motion(direction: Direction, offset: f64, total: f64) -> Motion {
    Motion::begin(7, viewport(direction, offset), total, at(0)).unwrap()
}
fn apply(motion: &mut Motion, now: Duration) -> Option<Attempt> {
    let attempt = motion.take(now).unwrap()?;
    motion.settle(attempt, Some(attempt.to()), now).unwrap();
    Some(attempt)
}

#[test]
fn invalid_bounds_offsets_and_begin_totals_are_refused() {
    for bad in [0., -1., 1_000_001., f64::NAN, f64::INFINITY] {
        assert!(Viewport::new(Direction::Down, bad, 2400., 0.).is_err());
        assert!(Viewport::new(Direction::Up, 800., bad, 0.).is_err());
    }
    for bad in [-1., 1600.01, f64::NAN, f64::INFINITY] {
        assert!(Viewport::new(Direction::Down, 800., 2400., bad).is_err());
    }
    for bad in [1_000_001., -1_000_001., f64::NAN, f64::INFINITY] {
        assert!(Motion::begin(7, viewport(Direction::Down, 0.), bad, at(0)).is_err());
    }
    assert!(Motion::begin(0, viewport(Direction::Down, 0.), 0., at(0)).is_err());
}

#[test]
fn the_session_scroll_action_is_supported_beside_its_planner() {
    let action = crate::config::WindowAction::ScrollStack;
    assert!(action.supported_session());
    assert!(crate::protocol::supported_session_action(
        &crate::config::Action::Hyprland { action }
    ));
}

#[test]
fn short_content_stays_at_the_native_rounded_center_without_dispatch() {
    for direction in [Direction::Down, Direction::Up] {
        for (content, center) in [(800., 0.), (200., -300.), (199., -301.)] {
            let viewport = Viewport::new(direction, 800., content, center).unwrap();
            assert_eq!(viewport.bounds(), (center, center));
            assert_eq!(viewport.offset(), center);
            let mut motion = Motion::begin(7, viewport, 600., at(0)).unwrap();
            assert!(motion.take(at(0)).unwrap().is_none());
            motion.progress(2, Phase::End, -600., at(1)).unwrap();
            assert_eq!(motion.state(), State::Finished);
            assert_eq!(motion.confirmed_offset(), center);
            assert!(motion.take(at(100)).unwrap().is_none());
        }
    }
    assert!(Viewport::new(Direction::Down, 800., 199., -300.5).is_err());
    assert!(Viewport::new(Direction::Down, 800., 200., 0.).is_err());
}

#[test]
fn initial_travel_and_effective_up_down_directions_follow_the_finger() {
    for (direction, to, native_move) in [(Direction::Down, 350., 50.), (Direction::Up, 450., -50.)]
    {
        let mut motion = motion(direction, 400., 50.);
        let attempt = apply(&mut motion, at(0)).unwrap();
        assert_eq!(attempt.ordinal(), 1);
        assert_eq!(attempt.from(), 400.);
        assert_eq!(attempt.to(), to);
        assert_eq!(attempt.layout_move(), native_move);
        motion.progress(3, Phase::End, 50., at(1)).unwrap();
        assert_eq!(motion.state(), State::Finished);
    }
}

#[test]
fn coalescing_retains_the_latest_cumulative_target_not_increment_sums() {
    let mut motion = motion(Direction::Down, 400., -20.);
    let first = motion.take(at(0)).unwrap().unwrap();
    for ordinal in 2..=10_000 {
        motion
            .progress(ordinal, Phase::Update, -f64::from(ordinal as u32), at(1))
            .unwrap();
        assert!(motion.take(at(1)).unwrap().is_none());
        assert_eq!(motion.in_flight(), Some(first));
    }
    motion.progress(10_005, Phase::End, -70., at(2)).unwrap();
    assert_eq!(motion.state(), State::Ending);
    motion.settle(first, Some(420.), at(3)).unwrap();
    assert!(motion.take(at(15)).unwrap().is_none());
    let last = apply(&mut motion, at(16)).unwrap();
    assert_eq!(last.ordinal(), 10_005);
    assert_eq!(last.from(), 420.);
    assert_eq!(last.to(), 470.);
    assert_eq!(last.layout_move(), -50.);
    assert_eq!(motion.state(), State::Finished);
    assert!(motion.take(at(100)).unwrap().is_none());
}

#[test]
fn ending_never_overtakes_the_in_flight_attempt_or_bypasses_pacing() {
    let mut motion = motion(Direction::Down, 400., -50.);
    let first = motion.take(at(0)).unwrap().unwrap();
    motion.progress(2, Phase::End, 0., at(1)).unwrap();
    assert_eq!(motion.state(), State::Ending);
    assert!(motion.take(at(100)).unwrap().is_none());
    motion.settle(first, Some(450.), at(101)).unwrap();
    let last = apply(&mut motion, at(101)).unwrap();
    assert_eq!((last.from(), last.to()), (450., 400.));
    assert_eq!(motion.state(), State::Finished);
    assert!(motion.progress(3, Phase::Update, 100., at(102)).is_err());

    let mut motion = self::motion(Direction::Down, 400., -50.);
    apply(&mut motion, at(0)).unwrap();
    motion.progress(2, Phase::End, -100., at(1)).unwrap();
    assert!(motion.take(at(15)).unwrap().is_none());
    apply(&mut motion, at(16)).unwrap();
    assert_eq!(motion.state(), State::Finished);
}

#[test]
fn finite_bounds_and_anchor_survive_both_extreme_displacement_directions() {
    for direction in [Direction::Up, Direction::Down] {
        for offset in [0., 333.25, 1600.] {
            let mut motion = motion(direction, offset, 0.);
            for (index, total) in [-1_000_000., 1_000_000., -125.5, 0.]
                .into_iter()
                .enumerate()
            {
                let now = at(index as u64 * 16);
                motion
                    .progress(index as u64 + 2, Phase::Update, total, now)
                    .unwrap();
                apply(&mut motion, now);
                let sign = if direction == Direction::Up { 1. } else { -1. };
                assert_eq!(
                    motion.confirmed_offset(),
                    (offset + sign * total).clamp(0., 1600.)
                );
            }
            assert_eq!(motion.confirmed_offset(), offset);
        }
    }
}

#[test]
fn dense_and_coalesced_delivery_have_the_same_anchored_destination() {
    for direction in [Direction::Up, Direction::Down] {
        for start in [0., 400., 1600.] {
            for total in -2000..=2000 {
                let mut dense = motion(direction, start, -35.);
                apply(&mut dense, at(0));
                dense.progress(2, Phase::Update, 9000., at(16)).unwrap();
                apply(&mut dense, at(16));
                dense
                    .progress(5, Phase::End, f64::from(total), at(32))
                    .unwrap();
                apply(&mut dense, at(32));

                let mut coalesced = motion(direction, start, -35.);
                coalesced
                    .progress(5, Phase::End, f64::from(total), at(32))
                    .unwrap();
                apply(&mut coalesced, at(32));
                assert_eq!(dense.state(), State::Finished);
                assert_eq!(coalesced.state(), State::Finished);
                assert_eq!(dense.confirmed_offset(), coalesced.confirmed_offset());
            }
        }
    }
}

#[test]
fn rejected_progress_does_not_replace_the_destination_or_end_the_gesture() {
    let mut motion = motion(Direction::Down, 400., 0.);
    motion.progress(5, Phase::Update, -100., at(10)).unwrap();
    for (ordinal, total, now) in [
        (5, 123., at(10)),
        (4, 123., at(11)),
        (0, 123., at(11)),
        (6, f64::NAN, at(11)),
        (6, f64::INFINITY, at(11)),
        (6, 1_000_001., at(11)),
        (6, -200., at(9)),
    ] {
        assert!(motion.progress(ordinal, Phase::End, total, now).is_err());
        assert_eq!(motion.state(), State::Active);
    }
    assert_eq!(apply(&mut motion, at(11)).unwrap().to(), 500.);
    motion.progress(6, Phase::End, -100., at(12)).unwrap();
    assert_eq!(motion.state(), State::Finished);
}

#[test]
fn cancel_discards_unsent_progress_without_a_rollback_mutation() {
    let mut motion = motion(Direction::Down, 400., -50.);
    apply(&mut motion, at(0));
    motion.progress(2, Phase::Update, -150., at(1)).unwrap();
    motion.cancel(at(2)).unwrap();
    motion.cancel(at(3)).unwrap();
    assert_eq!(motion.state(), State::Cancelled);
    assert_eq!(motion.confirmed_offset(), 450.);
    assert!(motion.take(at(100)).unwrap().is_none());
    assert!(motion.progress(3, Phase::End, -150., at(101)).is_err());
}

#[test]
fn cancel_does_not_pretend_to_retract_an_already_in_flight_mutation() {
    for observed in [Some(450.), None] {
        let mut motion = motion(Direction::Down, 400., -50.);
        let attempt = motion.take(at(0)).unwrap().unwrap();
        motion.cancel(at(1)).unwrap();
        assert_eq!(motion.in_flight(), Some(attempt));
        assert!(motion.take(at(100)).unwrap().is_none());
        motion.settle(attempt, observed, at(101)).unwrap();
        assert_eq!(
            motion.state(),
            if observed.is_some() {
                State::Cancelled
            } else {
                State::Uncertain
            }
        );
        assert!(motion.in_flight().is_none());
        assert!(motion.take(at(200)).unwrap().is_none());
    }
}

#[test]
fn missing_inexact_and_nonfinite_receipts_retire_without_retry() {
    for observed in [
        None,
        Some(400.),
        Some(447.99),
        Some(f64::NAN),
        Some(f64::INFINITY),
    ] {
        let mut motion = motion(Direction::Down, 400., -50.);
        let attempt = motion.take(at(0)).unwrap().unwrap();
        motion.progress(2, Phase::End, -100., at(1)).unwrap();
        motion.settle(attempt, observed, at(2)).unwrap();
        assert_eq!(motion.state(), State::Uncertain);
        assert_eq!(motion.confirmed_offset(), 400.);
        assert!(motion.in_flight().is_none());
        assert!(motion.take(at(1000)).unwrap().is_none());
        assert!(motion.progress(3, Phase::End, -100., at(1001)).is_err());
        assert!(motion.settle(attempt, Some(450.), at(1002)).is_err());
        motion.cancel(at(1003)).unwrap();
        assert_eq!(motion.state(), State::Uncertain);
    }
}

#[test]
fn foreign_repeated_and_old_receipts_never_settle_the_current_attempt() {
    let mut motion = motion(Direction::Down, 400., -50.);
    let first = motion.take(at(0)).unwrap().unwrap();
    let foreign = Motion::begin(8, viewport(Direction::Down, 400.), -50., at(0))
        .unwrap()
        .take(at(0))
        .unwrap()
        .unwrap();
    assert!(motion.settle(foreign, Some(450.), at(1)).is_err());
    assert_eq!(motion.in_flight(), Some(first));
    motion.settle(first, Some(450.), at(1)).unwrap();
    assert!(motion.settle(first, Some(450.), at(2)).is_err());
    motion.progress(2, Phase::Update, -100., at(2)).unwrap();
    let second = motion.take(at(16)).unwrap().unwrap();
    assert!(motion.settle(first, Some(450.), at(17)).is_err());
    assert_eq!(motion.in_flight(), Some(second));
    motion.settle(second, Some(500.), at(17)).unwrap();
}

#[test]
fn every_clock_boundary_is_monotonic_and_overflow_precedes_any_attempt() {
    let mut motion = motion(Direction::Down, 400., -50.);
    let attempt = motion.take(at(10)).unwrap().unwrap();
    assert!(motion.take(at(9)).is_err());
    assert!(motion.cancel(at(9)).is_err());
    assert!(motion.settle(attempt, Some(450.), at(9)).is_err());
    assert_eq!(motion.in_flight(), Some(attempt));
    motion.settle(attempt, Some(450.), at(10)).unwrap();
    let mut overflow =
        Motion::begin(7, viewport(Direction::Down, 400.), -50., Duration::MAX).unwrap();
    assert!(overflow.take(Duration::MAX).is_err());
    assert!(overflow.in_flight().is_none());
    assert_eq!(overflow.confirmed_offset(), 400.);

    let mut exhausted = self::motion(Direction::Down, 400., -50.);
    exhausted.sequence = u64::MAX;
    assert!(exhausted.take(at(0)).is_err());
    assert!(exhausted.in_flight().is_none());
    assert_eq!(exhausted.confirmed_offset(), 400.);
}

#[test]
fn truncated_receipts_within_tolerance_settle_at_the_exact_destination() {
    for observed in [448., 451.9] {
        let mut motion = motion(Direction::Down, 400., -50.);
        let attempt = motion.take(at(0)).unwrap().unwrap();
        motion.settle(attempt, Some(observed), at(1)).unwrap();
        assert_eq!(motion.state(), State::Active);
        assert_eq!(motion.confirmed_offset(), 450.);
    }
}

#[test]
fn fitted_destinations_name_the_offset_the_compositor_fits_to() {
    let ends: Vec<f64> = (1..=6).map(|strip| 400. * f64::from(strip)).collect();
    let fitted = viewport(Direction::Down, 400.)
        .fitted(ends.clone())
        .unwrap();
    for offset in [0., 123., 400., 1599.] {
        assert_eq!(fitted.native(offset), offset);
    }
    assert_eq!(fitted.native(1.), 0.);
    assert_eq!(fitted.native(401.5), 400.);
    let mut still = Motion::begin(7, fitted.clone(), -1.5, at(0)).unwrap();
    assert!(still.take(at(0)).unwrap().is_none());
    let mut moving = Motion::begin(8, fitted, -10., at(0)).unwrap();
    assert_eq!(moving.take(at(0)).unwrap().unwrap().to(), 410.);
    let tall = Viewport::new(Direction::Down, 800., 2400., 0.)
        .unwrap()
        .fitted(vec![1000., 2400.])
        .unwrap();
    assert_eq!(tall.native(0.), 100.);
    for ends in [
        vec![],
        vec![1200., 1200., 2400.],
        vec![1200., 2000.],
        vec![0., 2400.],
    ] {
        assert!(viewport(Direction::Down, 0.).fitted(ends).is_err());
    }
}
