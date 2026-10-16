# Custom recognizer proposal gate

`recognition::Gate` is the Rust ticket/response component for the planned Lua
recognizer path. It is not connected to public session requests, worker routing
or a Lua constructor yet. Those integrations remain required; this component
does not make custom recognizers available through `prek session`.

The trusted input coordinator constructs `Candidate` from its verified rule,
source generation, eligibility revision, expiry and current router ownership.
It must recheck policy, active session, context, frozen targets and conditions;
input selection alone does not establish eligibility. `Candidate` deliberately
has no deserializer. A session client cannot submit its own eligibility state.

A `Definition` names explicit selector IDs and normalized input kinds, declares
continuous/discrete behavior and bounds the response window to 1–250 ms. Names
are at most 128 bytes; selector/rule/seat IDs at most 256, without controls.
Definitions are unique and immutable within a gate's fence. A frame projects to
only declared selectors and input kinds; undeclared contact data is removed.
Neither a declaration nor a frame confers input or action authority.

`Gate::offer` creates one monotonically numbered ticket from an eligible frame.
The response deadline is bounded by the frame's original timestamp and the
candidate's expiry. Delivery/queue time cannot renew it. Repeated or regressing
frame sequences and candidate/recognizer changes fail. There are at most 64
declarations, 64 active candidates, 64 pending tickets, 64 KiB per encoded ticket
and 1 MiB of pending encoded tickets. A full gate rejects new work without
evicting live tickets.

`Gate::validate` consumes a known ticket and rechecks the current authoritative
candidate, source/eligibility revision, deadline and ownership. A forwarded or
draining source cannot validate a consuming response. A claimed source requires
that exact candidate to be the router's winner. Observation candidates cannot
be promoted into consuming candidates by changing their ownership category.
The response contains only the original fence/ticket, phase and bounded data;
it cannot supply a source, target, action, candidate or new deadline.

Discrete recognizers accept discrete or cancel decisions; continuous ones
require begin before updates/end. Pending responses produce no recognition.
Validated phases receive Rust-owned ordinals. Completed/cancelled candidates
cannot reopen, and accepting a newer reply excludes older replies. Returned
data is limited to eight nesting levels, bounded node traversal and 4 KiB after
JSON escaping. `retire`, `retire_source` and expiry remove pending work.

The result is a **validated proposal**, not a claim or execution grant. The
coordinator must still perform ordinary Rust arbitration, enforce its one-owner
rule and action permissions, and preserve fallback/draining behavior. It must
retire completed candidates, changed eligibility and lost sources. The Lua
callback, source-snapshot, authenticated broker/worker transport, cancellation
notifications and real routing integration are the next required work. Existing
production routing and public custom-recognizer negotiation remain disabled.
