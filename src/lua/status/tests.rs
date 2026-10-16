use super::*;
use crate::{
    lua::{Definition, EventKind},
    protocol::Epoch,
};

fn view(at: Duration) -> StatusView {
    StatusView {
        fence: Fence {
            broker: Epoch([u64::MAX, 1 << 63]),
            configuration: u64::MAX - 1,
            session: u64::MAX,
        },
        mode: Mode::Observe,
        state: State::Unknown,
        revision: u64::MAX,
        pending: 2,
        last_reload_error: Some("last good configuration retained".into()),
        context_error: Some("provider unavailable".into()),
        sampled_at: at,
    }
}
#[test]
fn status_copies_preserve_identities_and_original_expiry_without_desktop_context() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()
 local v=p.status.current()
 if v==nil then p.log.info('unavailable');return end
 assert(v.mode=='observe' and v.state=='unknown' and v.pending==2)
 assert(v.fence.broker[1]=='18446744073709551615' and v.fence.session=='18446744073709551615')
 assert(v.revision=='18446744073709551615' and v.context_error=='provider unavailable')
 v.pending=999;v.fence.session='0'
 assert(p.status.current().pending==2 and p.status.current().fence.session=='18446744073709551615')
 p.log.info('status')
end)
return {version=1}
"#,
        "status.lua",
    )
    .unwrap();
    let mut runtime = definition.instantiate().unwrap();
    assert_eq!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap()[0]
            .message,
        "unavailable"
    );
    let at = Duration::from_secs(1);
    runtime.set_status(view(at), at).unwrap();
    assert_eq!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap()[0]
            .message,
        "status"
    );
    runtime.set_clock(at + FRESHNESS).unwrap();
    assert_eq!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap()[0]
            .message,
        "unavailable"
    );
}
#[test]
fn status_updates_cannot_retarget_or_regress_and_configuration_cannot_set_status() {
    assert!(
        Definition::compile(
            "require('prek').status.current();return {version=1}",
            "phase.lua"
        )
        .is_err()
    );
    let at = Duration::from_secs(1);
    let mut runtime = Definition::compile("return {version=1}", "status.lua")
        .unwrap()
        .instantiate()
        .unwrap();
    assert!(runtime.set_status(view(at), Duration::ZERO).is_err());
    runtime.set_status(view(at), at).unwrap();
    let mut changed = view(at);
    changed.fence.session -= 1;
    assert!(runtime.set_status(changed, at).is_err());
    assert!(
        runtime
            .set_status(view(at - Duration::from_nanos(1)), at)
            .is_err()
    );
    let mut invalid = view(at);
    invalid.pending = usize::MAX;
    assert!(runtime.set_status(invalid, at).is_err());
    runtime.clear_status();
    assert!(runtime.host.borrow().status.is_none());
}

#[test]
fn status_and_capability_freshness_are_independent() {
    use crate::protocol::Capability;
    use std::collections::BTreeSet;
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function(event)
 assert((p.status.current()~=nil)==event.status)
 assert(p.capabilities.has('context')==event.capabilities)
end)
return {version=1}
"#,
        "separate-clocks.lua",
    )
    .unwrap();
    let at = Duration::from_secs(1);
    for refreshed in [true, false] {
        let mut runtime = definition.instantiate().unwrap();
        let mut status = view(at);
        runtime.set_status(status.clone(), at).unwrap();
        let mut capabilities = CapabilityView {
            fence: status.fence,
            negotiated: BTreeSet::from([Capability::Context]),
            ready: BTreeSet::new(),
            sampled_at: at,
        };
        runtime.set_capabilities(capabilities.clone(), at).unwrap();
        let later = at + Duration::from_millis(240);
        if refreshed {
            status.sampled_at = later;
            runtime.set_status(status, later).unwrap();
        } else {
            capabilities.sampled_at = later;
            runtime.set_capabilities(capabilities, later).unwrap();
        }
        runtime.set_clock(at + Duration::from_millis(260)).unwrap();
        runtime
            .dispatch(
                EventKind::Ready,
                &serde_json::json!({"status":refreshed,"capabilities":!refreshed}),
            )
            .unwrap();
    }
}
