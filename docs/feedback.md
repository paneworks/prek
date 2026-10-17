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
physical keyboard nor proves stock compositor, console or phone behavior.

Repeat-capable devices remain refused. Enabling `EV_REP` on a uinput clone can
introduce kernel software autorepeat in addition to forwarded physical repeats.
Repeat configuration, timing and exactly-once repeat delivery require their own
complete backend and kernel/consumer acceptance, not an LED bridge token.

The implementation follows upstream Linux v6.18
[input event disposition](https://github.com/torvalds/linux/blob/v6.18/drivers/input/input.c),
[uinput feedback](https://github.com/torvalds/linux/blob/v6.18/drivers/input/misc/uinput.c)
and [evdev client queries](https://github.com/torvalds/linux/blob/v6.18/drivers/input/evdev.c).
