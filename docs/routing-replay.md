# Offline downstream routing replay

`prek replay trace.jsonl --config config.lua --routing --check --json` runs the
native configured router without accessing devices, injecting input or executing
actions. The configuration must explicitly select `mode = 'route'`. Ordinary
`replay --config` remains recognition-only, even for a route-mode configuration.

Lua exposes the same driver as `p.replay.routing(config)`. Its `push`, `finish`,
`status` and `close` methods match `p.replay.new`; the result mode is `routing`.
Custom recognizers use fresh source-bound callback VMs and the shared replay VM
quota. They require runtime evaluation with the original approved source and
cannot nest inside another offline callback. Native-only traces also work during
configuration evaluation. These are value arguments to an offline handle, not
live policy edits or behavior registration.

## Trace format, version 1

Each JSONL line is one strict object tagged by `routing`, not `record`. Unknown
fields, unsupported versions, misplaced headers and records after expectations
are errors. Generic/live recording readers do not accept these objects.

Durations use native `{ "secs": 0, "nanos": 10000000 }` objects. Device,
generation, sequence and ticket values in JSON are exact unsigned integers;
Lua outcome tables expose opaque identities, including write tickets, as
canonical decimal strings. `result.json` retains the exact native JSON.

| `routing` | Fields besides the tag | Meaning |
|---|---|---|
| `header` | `version = 1` | Required first record. It initializes the inner native recording header. |
| `input` | `now`, `input` | Apply one ordinary native recording object. `now` is wall time; the inner frame/tick/context timestamp is the drained input watermark. |
| `attach` | `at`, `descriptor`, `consumer`, `feedback` | Simulate the prepared transport and its consumer acknowledgement at a synchronized neutral boundary. |
| `actions` | `at`, `seat`, `generation`, `capabilities` | Simulate available consumer capabilities for the exact recorded session generation. This grants no real action execution. |
| `poll` | `at` | Offer the next due raw frame per attached source and dequeue currently valid dispatch proposals. Repeated polls do not duplicate an outstanding offer. |
| `write` | `at`, `source`, `ticket` | Acknowledge complete simulated delivery of the exact outstanding frame. No bytes are written to a real device. |
| `retire` | `at`, `source` | Request native recovery of an attached source. It cancels its candidates and retires outstanding offers. |
| `expected` | `outcomes` | Required final downstream expectations when checking. Expectations include ordering and complete raw frames. |

`input.input` uses the ordinary `record` schema for `device`, `absolute`,
`frame`, `tick`, `mapping`, `reconcile`, `remove` and configured `context`.
It cannot contain another header or generic/rule expectations. For example:

```json
{"routing":"header","version":1}
{"routing":"input","now":{"secs":0,"nanos":10000000},"input":{"record":"tick","at":{"secs":0,"nanos":10000000}}}
{"routing":"poll","at":{"secs":0,"nanos":15000000}}
```

This fragment illustrates clocks; it is not a complete checked trace. A complete
trace registers input, supplies recorded context and admission, drives input/
output, retires every transport and specifies independently expected outcomes.
There is no implicit input, action consumer, write acknowledgement, timeout
advance or teardown at EOF.

## Admission and clocks

`descriptor` is the complete native `transport::Descriptor` snapshot, including
the original device. `consumer` is `transport::ConsumerEvidence`. Ordinary
descriptor, metadata, consumer identity/class/seat, freshness, feedback
and readiness validators apply. The descriptor must match the registered device
exactly. Admission requires synchronized neutral physical state, fresh recorded
desktop context and equal input/wall times. Held keys,
contacts, loss without reconciliation and stale source incarnations are refused.
An admitted source generation cannot be reused after retirement; register a
newer connection generation and supply fresh readiness.

These records are **simulation assertions**. A plausible PID, device path,
processed descriptor or `feedback = true` is not evidence that a process exists,
udev ran, a compositor accepted a device or a reverse feedback bridge works.
The driver performs no such I/O and returns no reusable live `Ready` token.

Wall time never moves backwards. Input timestamps never move backwards and
cannot exceed wall time. Wall-only polls/writes do not advance physical timers
past unread frames; `input` ticks explicitly advance the drained watermark.
Both clocks retain native deadline checks. Overdue writes fail instead of
silently repairing their timing. Recorded context must be refreshed normally;
expiry, lock state and session replacement retire affected transports and
pending write offers. Action availability expires independently, and dispatches
are validated when a poll dequeues them, not merely when recognition completes.

## Outcomes and checking

Outcomes are ordered strict objects tagged by `outcome`:

- `offered`: `{at, scheduled = {ticket, due, frame}}`. `frame` preserves the
  original source, timestamp, sequence and complete ordered raw event array,
  including native absolute-state repairs. An offer alone is not delivery.
- `delivered`: `{at, source, ticket}`. The corresponding outstanding offer was
  acknowledged exactly once at this simulated sink time.
- `dispatch`: `{at, event}`. The native typed action/progress proposal survived
  context, capability, source, generation and ownership fences at dequeue.
  Commands, power actions and Lua handlers in this proposal are never executed.
- `observation`: `{event}`. Native skipped/cancelled recognizer diagnostics;
  these do not authorize a dispatch or consume raw input.
- `recovery`: `{at, source, neutral_events,
  destroy_virtual_and_release_grab}`. The native router requests these cleanup
  operations. This does not prove kernel release or real neutralization.

`finish(true)` requires matching final expectations and fully retired transports.
Missing expectations, mismatch, active transports or malformed records fail
closed. Raw events, tickets, identities, timestamps, counts and order compare
exactly; only floating-point geometry permits the existing 1e-9 roundoff
tolerance. `finish(false)` is exploratory, not a passing acceptance test.

The CLI decoder limits lines to 4 MiB, input/output to 64 MiB, records to 100,000,
outcomes to 16,384, connected sources to 256, recorded seats to 16 and admitted
source history to 1,024. Lua retains the tighter shared handle/record/byte/source/
outcome limits in [the Lua API](lua-api.md#offline-replay). Both drivers share
the four-VM quota and 250 ms per-operation host work limit; normal Lua instruction,
memory and callback wall budgets remain unchanged. Split long Lua traces across
scheduler wakes. Errors, finish, close and VM retirement drop owned callback VMs.

This checks native downstream routing semantics. It does not replace supervised
worker IPC, authenticated provider, kernel transport, rendered application,
physical-input, phone, ARM or production pre/post-grab handoff acceptance.
