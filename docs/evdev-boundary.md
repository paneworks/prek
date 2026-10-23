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

The managed worker's separate state probe treats a node as neutral only with no
held key, no active switch and no contact. A virtual mirror starts with every
switch off and no switch baseline is replayed into it, so a node with an active
switch (for example a closed lid or tablet mode) stays ungrabbed and its
consumers keep reading the original node until every switch is off.

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

### Neutral idle takeover from stock Hyprland

For the stock Hyprland consumer the routing worker accepts an inferred
consumer-neutral boundary instead of a consumer acknowledgement, since stock
Hyprland exposes no per-device held state or processed-event cursor. After the
mirror's metadata, live consumer evidence, LED feedback and Morf admission pass,
the worker arms only when the source probe is neutral and no original input has
been read for 100 ms. One attempt then prepares the witness (pending events are
observed and the attempt waits again), grabs, and classifies. The readiness gate
is finished at the instant before the grab, which precedes every retained event.

A prefix that leaves a key, switch or contact active releases the grab before
commit and returns the whole prefix and tail through the observation loop;
committing would leave Hyprland holding that key with nothing to release it.
With an empty tail the release is lossless. A tail read in the same few
microseconds is not delivered to Hyprland after release; re-injecting it cannot
help, because the kernel already applied those transitions and drops events
that do not toggle its state. A failed grab, classification or readiness check
refuses that source until it is removed. A committed cut attaches the route when
the reader loop announces the delivered prefix.

Before offering consumer evidence for a mirror, the broker writes its Hyprland
per-device settings once, under the mirror's own device name: the original's
visible name-specific settings (XKB fields that differ from the global
`input:kb_*` values, an explicit mouse `scroll_factor`) plus the selector's
declared `hyprland` options, as one `keyword device[...]` batch for hyprlang or one
`r/eval hl.device{...}` for Lua, both applied before the reply. Evidence then
requires the declared settings stock IPC shows, and every other visible setting
to equal the original's. Declared settings stock IPC cannot show are trusted from
the write receipt. A failed write or check leaves the mirror without evidence,
so the worker refuses that source at its readiness deadline. A compositor
configuration reload discards runtime device entries; a reverted visible
setting fails the next evidence check, but invisible settings stay reverted
until a reload fence re-applies them.

Input-core grab acquisition does not itself wait for an RCU grace period.
Release does, and the console keyboard handler's `start` callback refreshes
LEDs; it does not reconstruct held keys. See the upstream
[console handler](https://github.com/torvalds/linux/blob/v6.18/drivers/tty/vt/keyboard.c).
Do not treat an ungrab as consumer-neutral resynchronization.

## Native axis representability

Worker-bound publication and virtual creation both check the native backend
before publishing metadata or opening `/dev/uinput`, respectively. The general
input/replay model still accepts up to 256 slots; native uinput accepts at most
100. Flat values must fit the axis span, calculated with wide signed arithmetic.
See the upstream [uinput validator](https://github.com/torvalds/linux/blob/v6.18/drivers/input/misc/uinput.c).

Type B initialization also resets slot/tracking flat values to zero and the
tracking-ID range to `0..65535`; incompatible metadata is refused rather than
silently changed. Resolution is preserved. See the
[slot initializer](https://github.com/torvalds/linux/blob/v6.18/drivers/input/input-mt.c).
The actual processed-metadata/ABI check remains required after creation.

`oslo make test-native-geometry` checks these boundaries using ordinary models
and private registry files. Its creation-order case deliberately invalidates
the private publication so a regressed guard fails before any device open.
This is not native kernel, compositor or routing acceptance.

## Nonzero fuzz is not an invertible transport

Linux filters absolute events against the previous value, including each Type B
slot. Forwarding an already filtered coordinate can filter it again. See the
upstream [input core](https://github.com/torvalds/linux/blob/v6.18/drivers/input/input.c).

With fuzz 16, raw 16 then 24 produces 8 then 16. Mirroring 8 from zero produces
2. A slot left at zero after consumption cannot reach 16 in one write: input 31
produces 15, while 32 produces 32. Clearing fuzz changes metadata; intermediate
updates change the observable stream. Neither is transparent restoration.

`oslo make test-axis-fuzz` checks bounded arithmetic counterexamples without
devices. The [user-launched root runner](root-routing.md) includes an ignored
native characterization expecting these values and checking an untouched slot,
neutral contacts and a zero-fuzz control. It uses only owned isolated nodes;
compilation and model results do not prove that native case executed. The
production nonzero-fuzz refusal remains in place.

## Per-device disable is not a neutral handoff

Hyprland v0.56.2 assigns the keyboard's `enabled` flag and ignores key/modifier
events while disabled. It does not thereby release the application's held
keys. Its pointer/libinput disable path is different. See its
[input manager](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/managers/input/InputManager.cpp).

`oslo make test-keyboard-quiesce` exercises this failure with one owned
Wayland-protocol keyboard and a real rendered application, using both legacy
and Lua Hyprland configuration. It selects the explicitly **test-only Aquamarine
Wayland startup fix**, not an unmodified-stock acceptance fixture. Host input,
evdev/uinput, console and production broker routing are not tested or altered.

The application receives a held C. After behaviorally qualifying disable, the
source releases C but the application stays held. After qualifying re-enable,
a fresh C press is also suppressed; only its later release restores neutrality.
Diagnostic Z batches confirm actual device behavior rather than trusting a
configuration reply or sleeping for an assumed refresh interval.

In this optional fixture mode, the receiver issues at most one `wl_display.sync`
at a time on a coalesced 20 ms timer. Only a request timestamp **after** the
keyboard producer's completed roundtrip fences a snapshot. An old request with
a late callback cannot qualify. This orders this controlled Wayland protocol
test; it is not an evdev/libinput drain acknowledgement for arbitrary apps.
Idle sync observations do not request redraws.

Every phase requires the actual application state and a rendered watermark.
Real captures must contain both the receiver's identity pixel and the matching
red-held/green-neutral pixel. Capture retries do not repeat input or configuration
effects; first unmatched images are retained. Five phase captures, full command/
synthetic-event trace and `keyboard-quiesce-result.json` remain under the attempt's
`target/cage-host/run-*`. A passing **negative** characterization explicitly
retains `production_route_tested=false` and `kernel_input_tested=false`.

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
admission/neutral/rollback/consumer/console obligations remain open. Its former
grab-and-blind-discard path has been removed: complete metadata/model snapshots
still return `Unsupported` before grabbing, draining, ungrabbing, attaching live
routing, asking Morf to activate, or emitting native `Ready`. Source-neutral queries are not promoted to
consumer-neutral acknowledgements. This is a safety refusal, not a completed
takeover protocol. A user-run
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
generation, retract actions already sent, prove neutral consumer re-entry or
establish physical hotplug/suspend acceptance. Polling and filesystem-check
overhead still need the full hardware/resource gates. Production routing remains
disabled.

`oslo make test-hotplug-discovery` exercises owned private files and database
replacements plus the actual source cursor, pending queue and recognition
engine. It tests full-description mismatches, missing initialization, stale
publication/node rejection, failed trust/bounds checks, cancellation of an old
hold, unaffected-source progress and held-state suppression before a genuinely
new press. It does not create or open kernel input devices.

## Virtual publication identities

Each kernel registry preparation appends a fresh 128-bit kernel-random identity
to the actual virtual device's physical marker. The matching protected pre-add
record and processed `PREK_PUBLICATION` property contain that full marker.
Recreating the same original source/generation does not reuse its publication
identity. Canonical base markers still identify the exclusive registry slot;
the stored record and import request must agree on the full publication marker.
An old helper request therefore cannot import a newer record from the same slot.

Published descriptors and consumer receipts carry the full marker. Validation
of a serialized descriptor only checks its shape; callers cannot submit an
already bound descriptor for kernel publication or choose its creation identity.
Prior publication metadata or consumer receipts cannot complete a new gate.
The private worker constructs feedback and readiness from the actual owned
publication's bound descriptor, not the unbound mirror from before publication.
The base descriptor is consumed by the registry; its per-creation identity is
then shared by both preparation paths.
The public descriptor JSON fields are unchanged. The identity is not secret,
an input permission, a consumer acknowledgement or a `Ready` token.

The processed-metadata gate uses the same bounded, protected, descriptor-bound
database parser as discovery. Missing initialization or an absent/stale
publication receipt remains pending. Duplicates, malformed/untrusted data or
matching-publication property mismatches fail. Actual virtual identity,
properties, event codes and axes must also match. Node/database and private
pre-add record identity are rechecked after ABI inspection. The private record
must retain its original inode as well as its exact contents.

Registry records now use version 2. Version-1 records are not imported, reused,
silently upgraded or automatically removed by recovery. Inspect any retained
legacy record and its original owner before attempting an upgrade; no migration
or cleanup is performed by building this package. The importer and producer
must come from the same package revision. This changes no physical-device rule
and triggers no existing device.

`oslo make test-virtual-publication` tests private registry/database files,
recreation, late import requests, strict receipts, replacement/legacy records,
consumer-evidence fencing and existing transport/discovery models. Actual
kernel cases remain in the user-launched runner. Neither layer establishes
stock-compositor/console drain, neutral handoff, physical source incarnation or
complete rollback. Production routing remains disabled.
