use super::*;
fn configured() -> (
    Routing,
    Device,
    Snapshot,
    crate::hyprland::layouts::Snapshot,
) {
    let mut device = device();
    device.axes.clear();
    device.properties.clear();
    device.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30])]);
    let mut cfg = config();
    cfg.policy.allow_hyprland_actions = true;
    let rule = &mut cfg.rules[0];
    rule.trigger = Trigger::Key {
        key: crate::config::Key(30),
    };
    rule.delivery = Delivery::Reserve;
    rule.output = Some("panel".into());
    rule.action = Action::Hyprland {
        action: crate::config::WindowAction::ReorderUp,
    };
    let mut routing = Routing::new(cfg).unwrap();
    context(&mut routing, 0);
    let (mut core, mut native) = crate::hyprland::reorder::tests::fixture(true);
    core.generation = routing.authority().generation();
    core.revision = 2;
    core.captured_at = ms(0);
    core.windows[1].focused = true;
    native.generation = core.generation;
    native.revision = 2;
    native.captured_at = ms(0);
    routing
        .authority_mut()
        .update_native(core.clone(), Some(native.clone()), ms(0))
        .unwrap();
    routing
        .attach(device.clone(), &ready(&device), None, ms(0))
        .unwrap();
    refresh(&mut routing);
    (routing, device, core, native)
}
fn refresh(routing: &mut Routing) {
    let generation = routing.authority().generation();
    routing.actions_available(
        generation,
        BTreeSet::from([Capability::HyprlandActions]),
        ms(0),
    );
}
fn edge(routing: &mut Routing, device: &Device, sequence: u64, value: i32) {
    routing
        .push(Record::Frame {
            frame: Frame {
                source: device.source.clone(),
                at: ms(sequence - 1),
                sequence,
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 30,
                        value,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            },
        })
        .unwrap();
}
#[test]
fn native_claim_is_frozen_before_dispatch_and_cannot_resurrect_after_neighbor_or_provider_loss() {
    for case in [
        "refresh",
        "neighbor",
        "loss-restored",
        "capability-restored",
    ] {
        let (mut routing, device, mut core, mut native) = configured();
        let original = routing.authority().reorder_scope("0x2:2", ms(0)).unwrap();
        edge(&mut routing, &device, 1, 1);
        assert!(drain(&mut routing, 0).is_empty());
        if case == "loss-restored" {
            core.revision += 1;
            routing.authority_mut().update(core.clone(), ms(0)).unwrap();
            refresh(&mut routing);
        }
        if case == "capability-restored" {
            routing.restrict_actions(&BTreeSet::new());
            refresh(&mut routing);
        }
        if case == "neighbor" {
            core.windows[0].id = "0x9:9".into();
            native.workspaces[0].windows[0].id = "0x9:9".into();
            native.workspaces[0].columns[0].members[0] = "0x9:9".into();
        }
        core.revision += 1;
        native.revision = core.revision;
        routing
            .authority_mut()
            .update_native(core.clone(), Some(native), ms(0))
            .unwrap();
        refresh(&mut routing);
        let dispatches = routing.take_scoped_dispatches(ms(0));
        if case == "refresh" {
            assert_eq!(dispatches.len(), 1);
            assert_eq!(dispatches[0].reorder_scope, Some(original.clone()));
        } else {
            assert!(dispatches.is_empty(), "{case} retargeted an owned gesture");
            if case != "neighbor" {
                assert_eq!(
                    routing.authority().reorder_scope("0x2:2", ms(0)),
                    Some(original.clone())
                );
            }
        }
        edge(&mut routing, &device, 2, 0);
        assert!(drain(&mut routing, 1).is_empty());
        let release = routing.take_scoped_dispatches(ms(1));
        if case == "refresh" {
            assert_eq!(release.len(), 1);
            assert_eq!(release[0].reorder_scope, Some(original));
            assert!(matches!(
                release[0].event,
                Event::Matched {
                    stage: crate::rules::Stage::End,
                    ..
                }
            ));
        } else {
            assert!(release.is_empty(), "{case} restored a retired release");
        }
    }
}
