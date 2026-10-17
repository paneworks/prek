use super::*;
mod native;
#[test]
fn queued_prepare_and_cancel_never_hide_a_boundary_or_revive_retired_state() {
    let state = Lifecycle::default();
    state.observe(true);
    state.observe(false);
    assert_eq!(
        state.value.get(),
        Snapshot {
            preparing: false,
            generation: 1
        }
    );
    state.observe(true);
    state.observe(true);
    state.observe(false);
    assert_eq!(
        state.value.get(),
        Snapshot {
            preparing: false,
            generation: 3
        }
    );
    state.failed.set(true);
    state.observe(true);
    state.observe(false);
    assert_eq!(state.value.get().generation, 3);
    assert!(state.failed.get());
}
#[test]
fn lifecycle_counters_cannot_wrap_into_an_old_incarnation() {
    let state = Lifecycle::default();
    state.value.set(Snapshot {
        preparing: false,
        generation: u64::MAX,
    });
    state.observe(true);
    assert!(state.failed.get());
    assert_eq!(state.value.get().generation, u64::MAX);
    let state = Lifecycle::default();
    state.events.set(u64::MAX);
    state.observe(false);
    assert!(state.failed.get());
}
