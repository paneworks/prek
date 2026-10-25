# Suspend-sensitive process authority

Input frames, protocol timestamps and existing deadlines retain their
`CLOCK_MONOTONIC` domain. This clock stops during system suspend, while
`CLOCK_BOOTTIME` includes suspended time. See the
[Linux clock contract](https://man7.org/linux/man-pages/man3/clock_gettime.3.html).

Every live Prek process maintains a guarded clock epoch. A sample reads
monotonic time before and after a boottime read, bounding the possible difference
between those clocks. Successive samples intersect those bounds. Scheduling
delays widen an individual measurement, but cannot widen the established epoch.
A confirmed disjoint offset, backwards/incoherent sample, failed clock query or
poisoned clock guard invalidates that process permanently. Returning to the old
offset does not restore it. No wall clock or arbitrary suspend-duration threshold
is used, and input timestamps are not rewritten.

The Linux reader, supervisor and IPC clock entry points share this guard. The
reader checks it before servicing callbacks or opening input. IPC send/receive
and Wayland request/receipt boundaries refuse work in a retired
epoch. An unchanged numerical deadline or previously queued packet therefore
cannot bypass a confirmed epoch loss. Already dispatched effects remain
uncertain and non-retryable; epoch loss cannot undo a command already handed to
another process.

Clock failures propagate as fatal `ConnectionAborted` errors, not an orderly
stop or a renewable lease. Supervisor destruction still kills and reaps its
owned child without needing a valid clock. Input teardown retains its existing
output-before-original closure obligations; clock failure is not itself a
release acknowledgement. A fresh process must establish new broker/session,
source and consumer authority. The packaged broker restarts on failure; a
one-shot foreground process must be relaunched from its proper login session.
`prek session --reconnect` retains an authority-free guardian in that login and
re-executes the agent on a classified clock/transport loss, after reaping the old
process. The guardian uses BOOTTIME only for bounded process lifecycle and never
uses or resets the guarded input/protocol epoch. A private one-byte pipe reports
an accepted broker registration, not source readiness or effect permission.
Fresh re-execution must still pass every broker policy, logind, executable,
compositor and provider check. Policy rejection and orderly exit stop the
launcher. The shared systemd user manager is not a substitute for owning logind
membership; only a compositor's own unit qualifies, while logind records that
compositor as its display session's controller. See [launcher bounds and stopping](nixos.md#start-the-owning-session-agent).

## Authoritative logind preparation

The production broker also watches `PrepareForSleep` on the fixed system bus
socket `/run/dbus/system_bus_socket`. It ignores environment-selected bus
addresses, authenticates logind's unique owner as UID 0 and pins that owner for
the watcher lifetime. A subscription and initial `PreparingForSleep` snapshot
cover startup during preparation. Queued prepare/cancel pairs retain a boundary
even when the final state is awake. Owner replacement, malformed notifications,
bus loss or an excessive work queue retire the watcher rather than rebinding it.

After configuration validation, the broker takes only a `sleep`/`delay`
inhibitor and rechecks the boundary before starting its worker. Startup while
preparing opens no input. A later boundary stops supervision before further
broker dispatch, reaps the input worker and closes compiler/control/broker
owners before releasing the inhibitor. The resource container preserves that
ordering on errors too. Resume or canceled sleep requires a fresh broker
process, not renewal of old authority. The existing systemd failure restart
provides that new process.

The same-login guardian uses a read-only watcher when
`session --reconnect --logind-library /protected/libsystemd.so.0` is selected.
The NixOS launcher supplies its configured library automatically. It stops and
reaps even an unregistered child across a boundary, waits while preparation is
active and clears only the reconnect outage budget. Its launch-rate bound is
not reset. It takes no inhibitor and grants no input/context authority.

This follows the [logind inhibitor contract](https://systemd.io/INHIBITOR_LOCKS/).
Delay is bounded by logind policy, not an indefinite block. It does not take
power/lid-key handling locks or change laptop policies. Forced sleep, low-level
kernel autosuspend and a stalled coordinator cannot be claimed covered by a
successful delay acquisition. Consumer neutral/rollback, actual hardware and
the complete lock/greeter/provider matrix remain separate gates; routing stays
disabled. The clock-discontinuity guard remains necessary.

### User-launched root API probe

```sh
oslo make build-root-sleep-watch   # build only, no privileged execution
oslo make test-sleep-watch-library # unprivileged ABI check, no bus access
./tests/run-root-sleep-watch.sh   # run yourself when ready
```

The script logs to `target/root-sleep-watch/latest/probe.log` and
`result.json`. Its root-side timeout is 15 seconds plus two seconds kill grace.
It authenticates the real system provider, acquires only its own temporary delay
inhibitor, finds its own PID/UID entry and verifies removal after closing it.
It does not request suspend, stop services, change VT or open input. This is an
API/descriptor-lifetime probe, not a suspend or consumer-release acceptance test.

## Limits and verification

`oslo make test-suspend-epoch` checks interval intersection, delayed and ambiguous
samples, signed offsets, equal/extreme timestamps, permanent retirement, query
failure and concurrent real clock reads. An isolated native child test confirms
that retired epochs fence old IPC and verified compositor sockets, refuse new
supervised work and still reap an existing owned child. Clock injection exists
only in the library test executable; production has no reset or test-clock flag.
These tests do not suspend the host or access input devices.
`oslo make test-session-reconnect` additionally exercises same-kernel-session
re-execution, single-child ordering, startup timeout, fatal/normal exits,
malformed readiness, SIGTERM reaping, SIGKILL parent-death protection and bounded
retry models. The native fixture runs on a genuine exec thread, not a library
test-harness thread, matching the [parent-death signal contract](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html).
A retired-epoch subprocess verifies that the guardian can launch
a fresh healthy child without restoring its own invalidated input clock.

Clock bracketing detects only discontinuities distinguishable from its sampling
uncertainty. It is not an authoritative pre-sleep notification, a proof of every
possible short suspend, or complete P08.3 recovery. Pre-sleep quiescence,
fresh source/consumer re-entry and actual hardware/lock/greeter suspend
acceptance remain required. Production routing
remains disabled, and consumer neutral/held-prefix/rollback gates are unchanged.
