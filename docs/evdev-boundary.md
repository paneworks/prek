# Evdev fanout-prefix witness

`linux::handoff::Handoff` classifies the evdev-client stream around a grab. It is
not wired into the production reader, and does not create a transport `Ready`,
context permit, neutral-state token or release acknowledgement. Production
routing remains disabled.

## Protocol

The caller supplies its exclusively owned, nonblocking, unfiltered evdev reader,
already using `CLOCK_MONOTONIC`, with no pending assembler data. `new` rejects a
reader already marked grabbed. It reopens the same node through the retained fd,
checks its device/inode binding and opens two independent clients: a monotonic
witness and a separate state-query probe. It never changes the original clock.

1. `prepare()` drains only the private witness's old history and reads the
   original once. `Observe(events)` returns every original event for the caller's
   ordinary observation path; it does not arm or grab. After the caller has
   processed that data, another prepare can return `Armed` only when the
   original read finds no input. New events after that empty read stay queued.
2. `grab()` acquires the original client's grab. Other consumers, policy and
   neutral-state checks remain the caller's responsibility.
3. `classify()` queries `EVIOCGKEY` on the **third** client, then drains the
   witness's frozen prefix. It compares original events exactly and in order,
   including timestamps, through that prefix. Equal timestamps and repeated
   identical frames do not change the event count. Any already-read suffix is
   retained. An empty prefix does not read the original at all.
4. `commit()` returns `Cut { observed, retained }` and leaves the grab with the
   original reader. `observed` has already been broadcast to other evdev
   clients: never inject it again. `retained` must precede later reads from the
   original. Events still queued on that reader remain untouched.

Dropping an uncommitted handoff attempts to release only the grab it acquired.
Read/acquisition/classification/phase failures poison the handoff; commit cannot
convert them into success. An ungrab error is not proof of descriptor release:
the caller must close the original and retain its existing teardown/acknowledgement
requirements. The owner must also handle panic, cancellation and process death.

Preparation and classification each bound reads to 4,096 calls, aggregate events
to 65,536 and loop work to 50 ms. Kernel syscall responsiveness is separate.
Interrupted reads retry within those limits. Synchronization loss, missing or
mismatched prefix, incomplete witness frame, resource exhaustion and malformed
phase ordering fail. A retained suffix may end in a partial frame; the caller's
existing assembler must preserve it rather than inventing a `SYN_REPORT`.

## Kernel assumptions and acceptance

This design is derived from the upstream Linux v6.12/v6.18 implementations, not
from a timing heuristic. `evdev_events` selects its client fanout and grab changes
that pointer. `evdev_handle_get_val` holds the input device's event lock before
flushing only the queried client's key queue. Input batch dispatch holds that
same event lock. The separate query is intended to serialize past pre-grab
fanout without flushing the original or witness. See the primary
[evdev implementation](https://github.com/torvalds/linux/blob/v6.18/drivers/input/evdev.c)
and [input core](https://github.com/torvalds/linux/blob/v6.18/drivers/input/input.c).

The input-core handler grab and evdev-client grab are different boundaries. This
witness does **not** establish console keyboard-handler delivery. It also does
not prove a stock compositor drained earlier events, an initially held sequence
can transfer between devices, a rollback preserved all ordinary input, or a
neutral snapshot is atomic. Those are remaining live handoff gates, not reasons
to enable CLI routing after a model pass.

`oslo make test-handoff-boundary` exercises the protocol with scripted ports,
including an in-flight pre-grab fanout and faults. These are models, not kernel
proof. Two new ignored kernel cases in the fixed user-launched runner challenge
the actual implementation on named synthetic sources:

- A REL-only source emits one pre-grab frame, bounded concurrent motion across
  grab and one definitely post-grab marker. The complete ordered stream must be
  preserved across the observed prefix, retained suffix and untouched queue.
- A keyboard emits a held press only after grabbing. An empty witness must leave
  the press queued despite the state query, then preserve its release.

The motion source has no key/switch capability and is marked libinput-ignored
before add. Keyboard events are never written while ungrabbed. Neither case
opens physical nodes or proves consumer/console/phone acceptance. Run the
[fixed privileged script](root-routing.md) yourself; the agent only observes its
logs. No positive kernel result is implied by building the runner or listing
its cases.
