# Native keyboard LED feedback

`linux::feedback::LedFeedback` connects the owning worker's virtual keyboard
back to its supplied original evdev descriptor. The private routing `Stream`
installs it for LED-capable devices, polls virtual feedback and pending physical
writes, and includes its snapshot/write deadlines in the actual reader loop.
The CLI's route gate remains closed. This implementation does not establish
live handoff, consumer/console readiness or physical keyboard acceptance.

## Descriptor and ownership boundary

The bridge reopens the retained original fd through `/proc/self/fd` as an
independent `O_WRONLY | O_NONBLOCK | O_CLOEXEC` client. Device/inode/rdev,
original connection liveness, kernel identity and exact LED capabilities must
match. It does not duplicate the grab-owning file description or resolve an
untrusted discovery path. A separate read-only client of the owned virtual
node verifies its marker/name/identity/LED capabilities before readiness.

LED state queries use these independent clients, never the forwarding reader.
Preflight can query and stage desired values but cannot write the original.
After activation, only declared `EV_LED` codes and boolean values are written.
The bridge accepts no arbitrary key, touch, force-feedback or repeat injection.
It is retired with the virtual output before the real reader is closed; its
clients do not keep the original descriptor's grab alive. Descriptor, I/O,
clock, malformed-feedback and readback failures retire the bridge permanently.

## Scheduling and recovery

Linux's uinput producer request ring is finite and can wrap without a
`SYN_DROPPED` notification. Producer requests therefore wake the bridge, but the
complete virtual LED snapshot supplies the authoritative desired state. A
250 ms periodic snapshot also recovers the latest state when the request ring
has wrapped to empty. Intermediate LED states may be coalesced. Requests are
not echoed into virtual input. Source state is queried separately and completed
writes are read back before their pending state is cleared.
The actual worker's passthrough write removes reverse LED fields, preserving
the source, sequence, time, key/event order, original report and delivery ticket.
Otherwise an old physical consumer's LED update could overwrite the virtual
consumer's desired state. Invalid fields/boundaries fail before any filtering;
an LED-only frame retains its original report, not an invented key or sync.

At most sixteen LED values and one aligned in-flight batch are retained. A new
snapshot does not overwrite a partially written batch: its remaining bytes
complete before the latest desired state is sent. Each attempt bounds write
syscalls, handles `EINTR`/`EAGAIN`, and preserves its exact byte cursor. Zero,
unaligned or faulted writes fail. A pending physical write has a 250 ms deadline
which subsequent updates cannot extend. `POLLOUT` is requested only while work
is pending, not on an idle always-writable evdev fd.
These bounds apply between syscalls. Kernel responsiveness and the independent
forwarding-loop supervisor remain necessary if a device operation stalls.

Feedback writes contain **no `SYN_REPORT`**. Appending one to a physical evdev
write could flush a hardware frame which the driver has not completed. The
bridge does not reset original input clocks or source frame sequences.

## Evidence and remaining gates

`oslo make test-feedback` exercises bounded state/writer behavior and independent
reopening without opening any input node. The existing ignored kernel test
`t55_isolated_keyboard_led_feedback_reaches_source_without_echo` now uses this
actual bridge rather than a manual relay. It challenges preflight non-mutation,
partial hardware frames, queued key releases, LED on/off, producer-ring wrap,
wrong-target retirement, conflicting original-consumer LED state without echo,
and independent grab lifetime.

That kernel case belongs to the fixed **user-launched**
[`./tests/run-root-routing.sh`](root-routing.md) inventory. Building/listing it
is not a positive kernel result. The agent does not launch it. It opens only
its named, pre-add libinput-ignored synthetic fixtures; it neither opens a
physical keyboard nor proves compositor, console or phone behavior.

## Repeat admission remains closed

This is a backend incompatibility, not just a missing LED-bridge branch. In Linux
v6.18, `uinput_create_device` registers a device without preset repeat values.
Neither uinput setup structure supplies those values; writes before creation
are legacy setup, not input events. `input_register_device` initializes software
repeat when both values are initially zero. Changing `REP_DELAY` or `REP_PERIOD`
after registration does not remove that timer function.

With nonzero values, forwarded key presses arm the virtual timer. Forwarding
the source's `EV_KEY` value 2 events can then add a second repeat stream. Dropping
those source events substitutes virtual timing for source timing. Removing
`EV_REP` changes capabilities. Setting delay or period to zero changes visible
settings, and a consumer's `EVIOCSREP` can enable the timer again. None of these
is an accepted transparent implementation. Zero-valued source settings are also
refused: their consumer can subsequently select nonzero values.
The descriptor requires the complete implicit `REP_DELAY`/`REP_PERIOD` pair;
`EV_REP` is not a partial code bitmap readable through `EVIOCGBIT(EV_REP)`.

The bound Wayland compositor is the one accepted exception. libinput discards kernel
repeat events and the compositor generates repeat itself and announces it to
clients through `wl_keyboard.repeat_info`, so a kernel-repeating keyboard whose
consumer is the bound compositor is mirrored without `EV_REP`
(`Descriptor::mirror_with_compositor_repeat`, flagged `compositor_repeat`). The
mirror has no software repeat timer; forwarded `EV_KEY` value 2 events still
reach other evdev readers, and `EV_REP` setting echoes are dropped before
writing. Console keyboards and other consumers stay refused.

The user-launched root runner includes one **negative characterization** case,
`kernel::tests::repeat::isolated_uinput_repeat_timer_and_consumer_settings_are_not_transparent`.
Its raw-uinput constructor exists only in `cfg(test)`; no production flag or
constructor bypass is added. It creates only a named synthetic keyboard with a
fresh pre-add publication and libinput-ignore metadata, verifies its actual
identity and protected completed publication, then grabs it before key injection.
It checks registration defaults, one explicitly emitted repeat with the timer
disabled, consumer setting updates and reverse producer requests, extra actual
timer repeats after re-enabling, and final neutral key state. Live settings use
`EVIOCGREP`, not evdev's cached `get_auto_repeat` opening snapshot.

Building or listing this ignored case is not executing it. A successful result
would characterize this incompatibility on the logged kernel, **not** prove
repeat passthrough, a physical source, compositor cadence or production routing.
The version-3 aggregate root receipt keeps `repeat_passthrough_tested=false` and
rejects missing, skipped or uncorrelated characterization results. Preserving
source repeat delivery, visible settings and reverse changes still needs a
complete backend and kernel/consumer acceptance before this admission gate opens.

The implementation follows upstream Linux v6.18
[input event disposition](https://github.com/torvalds/linux/blob/v6.18/drivers/input/input.c),
[uinput feedback](https://github.com/torvalds/linux/blob/v6.18/drivers/input/misc/uinput.c)
and [evdev client queries](https://github.com/torvalds/linux/blob/v6.18/drivers/input/evdev.c).
