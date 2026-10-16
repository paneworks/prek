use super::*;
use crate::{
    lua::CapabilityView,
    protocol::Capability,
    recognition::{InputKind, Metadata},
};
use std::collections::BTreeSet;

fn runtime(denied: Option<Capability>) -> Runtime {
    let mut definition = definition();
    definition
        .bind_recognizer(
            1,
            crate::recognition::Definition {
                name: "touch".into(),
                selectors: BTreeSet::from(["touch".into()]),
                inputs: BTreeSet::from([InputKind::Touch]),
                reply_within: crate::config::Millis(100),
                continuous: false,
            },
        )
        .unwrap();
    let mut runtime = definition.instantiate().unwrap();
    let view = crate::context_transfer::tests::view(Duration::ZERO);
    let mut negotiated = BTreeSet::from([
        Capability::CustomRecognizers,
        Capability::Context,
        Capability::LuaEvents,
        Capability::Observations,
    ]);
    if let Some(denied) = denied {
        negotiated.remove(&denied);
    }
    let caps = CapabilityView {
        fence: view.fence,
        sampled_at: Duration::ZERO,
        negotiated,
        ready: BTreeSet::new(),
    };
    runtime.set_context(view, Duration::ZERO).unwrap();
    runtime.set_capabilities(caps, Duration::ZERO).unwrap();
    runtime
}
fn update(generation: u64, reason: ResetReason) -> Metadata {
    let info = metadata(generation);
    Metadata {
        source: info.device.source.clone(),
        at: Duration::ZERO,
        selectors: info.selectors.clone(),
        reason,
        metadata: (reason != ResetReason::Removed).then(|| Box::new(info)),
    }
}
fn fence() -> Fence {
    crate::context_transfer::tests::view(Duration::ZERO).fence
}

#[test]
fn recognizer_catalog_updates_without_ordinary_input_and_retires_exact_sources() {
    let mut runtime = runtime(None);
    runtime
        .recognizer_metadata(fence(), &update(1, ResetReason::Attached))
        .unwrap();
    let logs = runtime
        .dispatch(EventKind::Ready, &serde_json::json!({}))
        .unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(
        logs[0].fields,
        serde_json::json!({"devices":true,"seat":true})
    );
    assert_eq!(runtime.host.borrow().devices.revision, 1);
    runtime
        .recognizer_metadata(fence(), &update(1, ResetReason::Reconciled))
        .unwrap();
    assert_eq!(runtime.host.borrow().devices.revision, 1);
    let mut mapping = update(1, ResetReason::MappingChanged);
    mapping
        .metadata
        .as_mut()
        .unwrap()
        .mapping
        .as_mut()
        .unwrap()
        .generation = 17;
    runtime.recognizer_metadata(fence(), &mapping).unwrap();
    assert_eq!(runtime.host.borrow().devices.revision, 2);
    runtime
        .recognizer_metadata(fence(), &update(2, ResetReason::Attached))
        .unwrap();
    assert_eq!(runtime.host.borrow().devices.revision, 3);
    runtime
        .recognizer_metadata(fence(), &update(1, ResetReason::Removed))
        .unwrap();
    assert_eq!(runtime.host.borrow().devices.entries.len(), 1);
    runtime
        .recognizer_metadata(fence(), &update(2, ResetReason::Removed))
        .unwrap();
    assert!(runtime.host.borrow().devices.entries.is_empty());
    assert_eq!(runtime.host.borrow().devices.revision, 4);
    assert!(runtime.host.borrow().devices.received);
}

#[test]
fn recognizer_metadata_rejects_missing_authority_fence_scope_and_forged_sources() {
    for cap in [
        Capability::CustomRecognizers,
        Capability::Context,
        Capability::LuaEvents,
        Capability::Observations,
    ] {
        let mut runtime = runtime(Some(cap));
        assert!(
            runtime
                .recognizer_metadata(fence(), &update(1, ResetReason::Attached))
                .is_err()
        );
        assert!(runtime.failed && runtime.host.borrow().devices.entries.is_empty());
    }
    for change in [
        "fence",
        "seat",
        "scope",
        "future",
        "stale",
        "generation",
        "capabilities",
    ] {
        let mut runtime = runtime(None);
        let mut metadata = update(1, ResetReason::Attached);
        let mut incoming = fence();
        match change {
            "fence" => incoming.session += 1,
            "seat" => {
                metadata.source.seat = "seat1".into();
                metadata.metadata.as_mut().unwrap().device.source = metadata.source.clone();
            }
            "scope" => {
                metadata.selectors = vec!["other".into()];
                metadata.metadata.as_mut().unwrap().selectors = metadata.selectors.clone();
            }
            "future" => metadata.at = Duration::from_millis(1),
            "stale" => runtime.set_clock(Duration::from_millis(251)).unwrap(),
            "generation" => metadata.source.generation = 0,
            "capabilities" => runtime.clear_capabilities(),
            _ => unreachable!(),
        }
        assert!(
            runtime.recognizer_metadata(incoming, &metadata).is_err(),
            "{change}"
        );
        assert!(runtime.failed && runtime.host.borrow().devices.entries.is_empty());
    }
}

#[test]
fn custom_catalog_queries_hide_when_capabilities_expire_with_context_still_fresh() {
    let mut runtime = runtime(None);
    runtime
        .recognizer_metadata(fence(), &update(1, ResetReason::Attached))
        .unwrap();
    runtime
        .set_context(
            crate::context_transfer::tests::view(Duration::from_millis(251)),
            Duration::from_millis(251),
        )
        .unwrap();
    let logs = runtime
        .dispatch(EventKind::Ready, &serde_json::json!({}))
        .unwrap();
    assert_eq!(
        logs[0].fields,
        serde_json::json!({"devices":false,"seat":true})
    );
    runtime.clear_capabilities();
    let logs = runtime
        .dispatch(EventKind::Ready, &serde_json::json!({}))
        .unwrap();
    assert_eq!(logs[0].fields["devices"], false);
}
