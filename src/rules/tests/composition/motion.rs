//! Private custom-to-native origin, phase and lifecycle models.
use super::*;

pub(super) fn swipe(continuous: bool) -> Trigger {
    Trigger::Swipe {
        fingers: 2,
        direction: Direction::Right,
        travel: crate::config::Distance::Millimetres(12.0),
        fallback_travel: crate::config::Distance::Normalized(0.04),
        continuous,
    }
}
fn native(last: Trigger) -> Engine {
    model_steps(touch_custom("first"), last, 1, true)
}
fn start(engine: &mut Engine) -> Ticket {
    touch_start(engine, 200);
    let tickets = engine.recognition_requests(ms(0)).unwrap();
    let [ticket] = &tickets[..] else {
        panic!("{tickets:?}")
    };
    ticket.clone()
}
fn points(
    engine: &mut Engine,
    at: u64,
    sequence: u64,
    first: (i32, i32),
    last: (i32, i32),
    end: bool,
) {
    shape_touch(
        engine,
        at,
        sequence,
        &[(0, first.0, first.1), (1, last.0, last.1)],
        false,
        end,
    );
}
fn complete(engine: &mut Engine, ticket: &Ticket, at: u64) {
    engine
        .recognition_response(reply(ticket, CustomPhase::Discrete), ms(at))
        .unwrap();
}
fn final_touch(event: &Event) -> &TouchGesture {
    let Event::Matched {
        evidence: Evidence::Sequence { steps },
        ..
    } = event
    else {
        panic!("{event:?}")
    };
    let Evidence::Touch { gesture, .. } = steps.last().unwrap() else {
        panic!("{steps:?}")
    };
    gesture
}

#[test]
fn swipe_rebases_old_motion_and_emits_one_new_begin_update_end_identity() {
    let mut engine = native(swipe(true));
    let ticket = start(&mut engine);
    let outer = engine.sequences[0].pending["seat0"].id;
    points(&mut engine, 20, 2, (400, 500), (500, 500), false);
    assert!(matches(&engine.events).is_empty());
    complete(&mut engine, &ticket, 30);
    assert_eq!(engine.input_watermark, 2);
    let candidate = &engine.sequences[0].steps[1].workers[0].candidates[&device(1).source];
    assert_eq!(candidate.started, ms(30));
    assert_eq!(candidate.continued_from, Some(ticket.candidate));
    assert!(engine.sequences[0].pending["seat0"].admits_input(candidate));
    points(&mut engine, 40, 3, (400, 500), (500, 500), false);
    assert!(matches(&engine.events).is_empty());
    points(&mut engine, 50, 4, (700, 500), (800, 500), false);
    points(&mut engine, 60, 5, (800, 500), (900, 500), false);
    points(&mut engine, 70, 6, (800, 500), (900, 500), true);
    let matched = matches(&engine.events);
    assert_eq!(matched.len(), 3, "{:?}", engine.events);
    for (event, (ordinal, stage)) in
        matched
            .iter()
            .zip([(1, Stage::Begin), (2, Stage::Update), (3, Stage::End)])
    {
        assert!(
            matches!(event,Event::Matched{id,ordinal:actual,stage:actual_stage,..}
            if *id==outer && *actual==ordinal && *actual_stage==stage)
        );
    }
    assert!(
        matches!(final_touch(matched[0]),TouchGesture::Pan{total,delta,velocity,..}
        if (total.x-300.0).abs()<1e-9 && total==delta && (velocity.x-15000.0).abs()<1e-6)
    );
    assert!(
        matches!(final_touch(matched[1]),TouchGesture::Pan{total,delta,..}
        if (total.x-400.0).abs()<1e-9 && (delta.x-100.0).abs()<1e-9)
    );
    assert_eq!(engine.input_watermark, 6);
    assert!(engine.sequences[0].pending.is_empty());
}

#[test]
fn releasing_after_only_pre_reply_motion_cannot_complete_a_discrete_swipe() {
    let mut engine = native(swipe(false));
    let ticket = start(&mut engine);
    points(&mut engine, 20, 2, (700, 500), (800, 500), false);
    complete(&mut engine, &ticket, 30);
    points(&mut engine, 40, 3, (700, 500), (800, 500), true);
    assert!(matches(&engine.events).is_empty());
    assert_eq!(engine.sequences[0].pending["seat0"].step, 1);
}

#[test]
fn pinch_and_rotation_use_the_handoff_geometry_not_the_original_birth_geometry() {
    for rotate in [false, true] {
        let mut engine = native(if rotate {
            Trigger::Rotate {
                fingers: 2,
                degrees: 30.0,
            }
        } else {
            Trigger::Pinch {
                fingers: 2,
                ratio: 0.2,
            }
        });
        let ticket = start(&mut engine);
        let (first, last) = if rotate {
            ((150, 450), (150, 550))
        } else {
            ((50, 500), (350, 500))
        };
        points(&mut engine, 20, 2, first, last, false);
        complete(&mut engine, &ticket, 30);
        points(&mut engine, 40, 3, first, last, false);
        assert!(matches(&engine.events).is_empty());
        let (first, last) = if rotate {
            ((200, 500), (100, 500))
        } else {
            ((0, 500), (400, 500))
        };
        points(&mut engine, 50, 4, first, last, false);
        points(&mut engine, 60, 5, first, last, true);
        let matched = matches(&engine.events);
        assert_eq!(matched.len(), 3, "rotate={rotate}; {:?}", engine.events);
        for (event, stage) in matched
            .iter()
            .zip([Stage::Begin, Stage::Update, Stage::End])
        {
            assert!(matches!(event,Event::Matched{stage:actual,..} if *actual==stage));
        }
        if rotate {
            assert!(
                matches!(final_touch(matched[0]),TouchGesture::Rotate{degrees,..} if (*degrees-90.0).abs()<1e-6)
            );
        } else {
            assert!(
                matches!(final_touch(matched[0]),TouchGesture::Pinch{ratio,..} if (*ratio-4.0/3.0).abs()<1e-9)
            );
        }
        assert!(engine.sequences[0].pending.is_empty());
    }
}

#[test]
fn shape_handoff_discards_the_eager_path_and_seeds_only_current_centroid() {
    let mut engine = native(Trigger::Shape {
        name: "v".into(),
        fingers: Some(2),
        button: None,
    });
    let ticket = start(&mut engine);
    points(&mut engine, 20, 2, (450, 900), (550, 900), false);
    points(&mut engine, 30, 3, (100, 100), (200, 100), false);
    complete(&mut engine, &ticket, 40);
    let worker = &engine.sequences[0].steps[1].workers[0];
    let Recognizer::Shape { captures, .. } = &worker.recognizer else {
        unreachable!()
    };
    let capture = &captures[&device(1).source];
    assert_eq!(capture.started, ms(40));
    assert_eq!(capture.path.as_ref().unwrap().sample_count(), 1);
    assert_eq!(
        capture.identity.contacts,
        worker.candidates[&device(1).source].contacts
    );
    points(&mut engine, 50, 4, (450, 900), (550, 900), false);
    points(&mut engine, 60, 5, (800, 100), (900, 100), true);
    assert!(
        matches!(&matches(&engine.events)[..],[Event::Matched{evidence:Evidence::Sequence{steps},..}]
        if matches!(&steps[1],Evidence::Shape{shape,..} if shape.name=="v")),
        "{:?}",
        engine.events
    );
}

#[test]
fn native_motion_handoff_preserves_the_first_target_after_focus_and_position_move() {
    let mut engine = native(swipe(true));
    let action = Action::Hyprland {
        action: crate::config::WindowAction::Close,
    };
    engine.sequences[0].rule.action = action.clone();
    engine.sequences[0].rule.output = Some("phone".into());
    for child in &mut engine.sequences[0].steps {
        child.workers[0].rule.action = action.clone();
        child.workers[0].rule.output = Some("phone".into());
    }
    let ticket = start(&mut engine);
    points(&mut engine, 20, 2, (600, 500), (700, 500), false);
    let mut changed = snapshot(1, 2, 25);
    changed.windows[0].focused = false;
    changed.windows[1].focused = true;
    engine.authority.update(changed, ms(25)).unwrap();
    complete(&mut engine, &ticket, 30);
    let candidate = &engine.sequences[0].steps[1].workers[0].candidates[&device(1).source];
    assert_eq!(candidate.permit.as_ref().unwrap().window().unwrap().id, "a");
    points(&mut engine, 40, 3, (800, 500), (900, 500), false);
    assert!(
        matches!(&matches(&engine.events)[..],[Event::Matched{window:Some(window),..}] if window=="a")
    );
}

#[test]
fn native_motion_and_shape_continuations_cancel_on_source_context_and_helper_loss() {
    for shape in [false, true] {
        for reset in 0..7 {
            let mut engine = native(if shape {
                Trigger::Shape {
                    name: "v".into(),
                    fingers: Some(2),
                    button: None,
                }
            } else {
                swipe(true)
            });
            let ticket = start(&mut engine);
            complete(&mut engine, &ticket, 10);
            match reset {
                0 => frame(
                    &mut engine,
                    device(1).source,
                    20,
                    2,
                    &[(EV_SYN, crate::input::SYN_DROPPED, 0)],
                ),
                1 => {
                    let mut mapping = engine.devices[&1].mapping.clone().unwrap();
                    mapping.generation += 1;
                    engine
                        .push(Record::Mapping {
                            source: device(1).source,
                            at: ms(20),
                            mapping,
                        })
                        .unwrap();
                }
                2 => engine
                    .push(Record::Reconcile {
                        source: device(1).source,
                        at: ms(20),
                        keys: BTreeSet::new(),
                        contacts: engine.devices[&1].contacts.clone(),
                    })
                    .unwrap(),
                3 => engine
                    .push(Record::Remove {
                        source: device(1).source,
                        at: ms(20),
                    })
                    .unwrap(),
                4 => engine.recognition_ready(false, ms(20)).unwrap(),
                5 => engine.retire_routing_recognition(&device(1).source),
                _ => {
                    engine.authority.lock_requested();
                    tick(&mut engine, 20);
                }
            }
            assert!(
                engine.sequences[0].pending.is_empty(),
                "shape={shape},reset={reset}"
            );
            assert!(matches(&engine.events).is_empty());
        }
    }
}

#[test]
fn delayed_native_motion_admission_does_not_advance_the_drained_input_clock() {
    for shape in [false, true] {
        let mut engine = native(if shape {
            Trigger::Shape {
                name: "v".into(),
                fingers: Some(2),
                button: None,
            }
        } else {
            swipe(true)
        });
        let ticket = start(&mut engine);
        engine
            .recognition_response_after_input(reply(&ticket, CustomPhase::Discrete), ms(0), ms(30))
            .unwrap();
        assert_eq!(engine.input_at(), ms(0));
        assert_eq!(engine.input_watermark, 1);
        assert_eq!(
            engine.sequences[0].steps[1].workers[0].candidates[&device(1).source].started,
            ms(30)
        );
        points(&mut engine, 20, 2, (700, 500), (800, 500), false);
        assert!(matches(&engine.events).is_empty());
        assert!(
            engine.sequences[0].pending.is_empty(),
            "backdated motion was not retired: {:?}",
            engine.events
        );
    }
}

#[test]
fn native_continuation_starts_nested_deadlines_at_admission_not_contact_birth() {
    let mut engine = native(swipe(true));
    engine.sequences[0].groups.push(SequenceGroup {
        first: 1,
        end: 2,
        timeout: ms(50),
    });
    let ticket = start(&mut engine);
    complete(&mut engine, &ticket, 30);
    assert_eq!(engine.sequences[0].pending["seat0"].groups[&1], ms(30));
    tick(&mut engine, 79);
    assert_eq!(engine.sequences[0].pending["seat0"].step, 1);
    tick(&mut engine, 80);
    assert!(engine.sequences[0].pending.is_empty());
}

#[test]
fn a_completed_custom_touch_group_can_wait_for_fresh_custom_and_native_groups() {
    for native_last in [false, true] {
        let mut engine = if native_last {
            native(swipe(true))
        } else {
            model_steps(touch_custom("first"), touch_custom("last"), 1, true)
        };
        let ticket = start(&mut engine);
        points(&mut engine, 1, 2, (100, 500), (200, 500), true);
        complete(&mut engine, &ticket, 2);
        assert_eq!(engine.sequences[0].pending["seat0"].step, 1);
        assert!(engine.recognition_requests(ms(3)).unwrap().is_empty());
        touch_start_at(&mut engine, 200, 4, 3);
        if native_last {
            points(&mut engine, 30, 4, (500, 500), (600, 500), false);
            assert_eq!(matches(&engine.events).len(), 1);
        } else {
            let tickets = engine.recognition_requests(ms(4)).unwrap();
            let [last] = &tickets[..] else {
                panic!("{tickets:?}")
            };
            assert_eq!(last.cause, crate::recognition::Cause::Frame);
            assert_eq!(last.recognizer, "last");
            complete(&mut engine, last, 5);
            assert_eq!(matches(&engine.events).len(), 1);
        }
    }
}

#[test]
fn a_custom_final_step_keeps_its_original_proposal_expiry_on_the_parent_outcome() {
    let mut engine = model_steps(Trigger::Key { key: Key(30) }, custom("last"), 1, true);
    engine.sequences[0].routing = true;
    key(&mut engine, 1, 0, 1, 30, true);
    let tickets = engine.recognition_requests(ms(0)).unwrap();
    let last = tickets
        .iter()
        .find(|ticket| ticket.recognizer == "last")
        .unwrap();
    complete(&mut engine, last, 1);
    let outcomes = engine.take_route_outcomes();
    let [outcome] = &outcomes[..] else {
        panic!("{}", outcomes.len())
    };
    assert_eq!(outcome.expires, Some(last.deadline));
    assert!(last.deadline > ms(1));
}

#[test]
fn asynchronous_shape_activation_rechecks_the_shared_sample_budget_and_retires_on_overflow() {
    let mut engine = native(Trigger::Shape {
        name: "v".into(),
        fingers: Some(2),
        button: None,
    });
    let ticket = start(&mut engine);
    let identity = CaptureIdentity {
        source: Source {
            device: 999,
            generation: 1,
            seat: "seat0".into(),
        },
        mapping_generation: 0,
        contacts: BTreeSet::new(),
    };
    let mut path = PathCapture::new_relative(
        identity.clone(),
        ms(0),
        crate::paths::RelativeLimits {
            max_samples: 65_536,
            timeout: ms(1000),
            minimum_length: 1.0,
            minimum_size: 1.0,
            armed_throughout: false,
        },
        true,
    )
    .unwrap();
    for _ in 1..65_536 {
        path.sample_relative(&identity, ms(0), Point { x: 1.0, y: 0.0 }, true)
            .unwrap();
    }
    let Recognizer::Shape { captures, .. } =
        &mut engine.sequences[0].steps[1].workers[0].recognizer
    else {
        unreachable!()
    };
    captures.clear();
    captures.insert(
        identity.source.clone(),
        ShapeCapture {
            started: ms(0),
            path: Some(path),
            identity,
        },
    );
    assert_eq!(engine.shape_samples(), 65_536);
    let error = engine
        .recognition_response(reply(&ticket, CustomPhase::Discrete), ms(10))
        .unwrap_err();
    assert!(error.to_string().contains("budgets"));
    assert!(engine.sequences[0].pending.is_empty());
    assert!(engine.shape_samples() <= 65_536);
    assert!(matches(&engine.events).is_empty());
}
