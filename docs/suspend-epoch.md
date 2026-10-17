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
and Hyprland/Morf native request/receipt boundaries refuse work in a retired
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
membership. See [launcher bounds and stopping](nixos.md#start-the-owning-session-agent).

## Limits and verification

`oslo make test-suspend-epoch` checks interval intersection, delayed and ambiguous
samples, signed offsets, equal/extreme timestamps, permanent retirement, query
failure and concurrent real clock reads. An isolated native child test confirms
that retired epochs fence old IPC and verified Hyprland sockets, refuse new
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
