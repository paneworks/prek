# Evdev fanout-prefix witness

`linux::handoff::Handoff` classifies the evdev-client stream around a grab. It
does not create a transport `Ready`,
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

## Read failures

Errors from `prepare`, `grab`, `classify` and `commit` retain their error kind
and carry a downcastable `linux::handoff::ReadFailure` payload. `journal()`
exposes every original event consumed by that attempt, in read order. It also
records the number of exactly matched prefix events, the witness event count,
whether the synchronized witness was completely validated, and whether this
attempt acquired a grab. `into_parts()` returns the original cause and journal;
the cause retains its OS error information. No event or report is invented.

The final batch is recorded before post-read budget, synchronization and prefix
validation. A mismatch, mid-prefix read failure or exceeded budget cannot silently
discard an already-read tail. Successful `Observe` and `Cut` results transfer
their data normally; a later failed attempt does not duplicate prior observations.
The normal event/read/work budgets still apply; a failed read's already-returned
final batch is retained even when it is the batch that exceeded the budget.

The journal is private process data, not an IPC payload or rollback permission.
Display and Debug output include no raw events. `grab_acquired()` is historical,
not proof of current ownership, successful ungrab or consumer state. Neither a
matched prefix nor its journal can repair a consumer automatically. Unchecked
data, `SYN_DROPPED`, incomplete prefixes, native consumer acknowledgement and
complete rollback still need their explicit lifecycle handling. Production
routing remains disabled.

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
  A second attempt then fails after actual classification; its error journal
  plus the untouched reader queue must preserve the complete ordered stream.
- A keyboard emits a held press only after grabbing. An empty witness must leave
  the press queued despite the state query, then preserve its release.

The motion source has no key/switch capability and is marked libinput-ignored
before add. Keyboard events are never written while ungrabbed. Neither case
opens physical nodes or proves consumer/console/phone acceptance. Run the
[fixed privileged script](root-routing.md) yourself; the agent only observes its
logs. No positive kernel result is implied by building the runner or listing
its cases.

## Sole-reader ingress

The Linux reader loop accepts `ReaderDisposition::Observe(events)` and
`ReaderDisposition::Cutover(cut)` from an explicitly opting-in trusted sink.
Both feed the **original** source assembler; sequence numbers are not reset.
Observed-prefix frames reach `InputSink::push` first. Only after their successful
delivery does `reader_cutover(source, through, at)` announce the boundary before
the first retained/post-prefix frame. `at` is the already-delivered input clock,
not wall time, the next unread frame's time or the enclosing poll watermark.
If no complete tail exists, the loop announces a ready boundary before polling.

A retained partial frame waits for its real `SYN_REPORT`. No contact/key starts,
timestamps or synthetic reports are invented. A future-dated prefix remains in
the ordinary bounded pending queue until the input watermark reaches it; a
future frame supplies a real poll deadline instead of a zero-timeout busy loop.
Repeated empty drains do not re-sort or copy an unchanged future queue.

The pending queue bounds both complete frames and raw events to 65,536 across
all sources and hook/kernel ingress. Per-frame input is limited to 4,096 events;
ordinary reads also have bounded event/read counts. Synchronization loss retires
the source immediately, even without a following report. Clock regression and
input preceding the delivered watermark cannot dispatch. Pending failed-source
frames are removed, and cancellation is delivered at a safe input-clock point
before healthy-source frames, without advancing timers through unread input.
Output shutdown still precedes actual original-fd closure and release acknowledgement.

`reader_cutover` defaults to refusal; it is not a readiness or policy grant.
The private routing `Stream` does not yet opt into this interface, and its live
admission/neutral/rollback/consumer/console obligations remain open. A user-run
synthetic loop case checks prefix/callback/tail ordering, one downstream output
and output-before-original-close teardown. Library tests exercise the same
assembler, queue, dispatcher and cancellation helpers without opening devices.

## Discovery and source re-entry

Discovery reads udev metadata using the opened descriptor's `fstat` device
number. It does not derive another device's properties by resolving the event
pathname again. One publication supplies both initialization and properties.
The reader binds its device/inode/character-device number and the database
file's inode, length and modification/change times. The public inventory JSON
shape is unchanged; inaccessible or invalid entries remain explicit failures.

The database must be a root-owned regular file without group/world write
access. Reads are nonblocking, refuse leaf symlinks and accept at most 64 KiB. Parsing
bounds properties to 1,024, property keys and seats to 256 bytes, rejects
duplicate initialization/property records, invalid UTF-8 and incomplete final
lines. Unknown record kinds remain compatible with database extensions.
An absent database permits explicit diagnostic `watch` use with the default
seat, but does not establish managed-device initialization.

Managed admission requires an initialized publication and the same complete
device description at discovery and open: seat, identity, name, path, physical
and unique identifiers, vendor/product, properties, capabilities, axes and
virtual-device status. Only the newly assigned source generation may differ.
Node and publication checks run before reader hooks, around draining and after
startup state queries. Replaced, removed or republished metadata retires the old
source, removes its pending frames and cancels its recognizers. Rediscovery
assigns a fresh generation and queries held state without inventing new presses
or contact starts. Unaffected sources keep their own lifetimes.

Systemd publishes `I:` initialization and `E:` properties together using a
temporary-file rename; see its
[database writer](https://github.com/systemd/systemd/blob/v259.3/src/libsystemd/sd-device/device-private.c).
That is a metadata publication boundary, not an atomic input/consumer boundary.
An initialized file at a reused event minor does not itself prove that udev
completed the new kernel incarnation; authoritative add/remove reconciliation
remains required. These checks do not serialize udev updates with kernel event
generation,
retract actions already sent, prove neutral consumer re-entry or establish
physical hotplug/suspend acceptance. Polling and filesystem-check overhead still
need the full hardware/resource gates. Production routing remains disabled.

`oslo make test-hotplug-discovery` exercises owned private files and database
replacements plus the actual source cursor, pending queue and recognition
engine. It tests full-description mismatches, missing initialization, stale
publication/node rejection, failed trust/bounds checks, cancellation of an old
hold, unaffected-source progress and held-state suppression before a genuinely
new press. It does not create or open kernel input devices.
