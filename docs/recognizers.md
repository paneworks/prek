# Custom recognizer proposal gate

`recognition::Gate` is the Rust ticket/response component for the planned Lua
recognizer path. An internal host binding now connects approved source callbacks
through the supervised Lua helper. Public session requests, worker routing and
the source-level constructor remain required; custom recognizers are not yet
available through `prek session`.

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

`offer_timer` wakes an existing, still eligible candidate when physical input is
stationary. The coordinator must check the current held state and conditions.
The ticket has `cause=timer`, a new Rust `requested_at` and response deadline,
and retains the last physical frame's timestamp and sequence. Changes, contact
births/releases and ended positions are cleared; retained contacts are marked
non-fresh. This never extends a previous ticket. Repeated wake times, changed
candidates and retired sources fail. Scheduling approved wakes and delivering
candidate/source cancellation notifications remain coordinator integration work.

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
retire completed candidates, changed eligibility and lost sources. Existing
production routing and public custom-recognizer negotiation remain disabled.

The preparatory Rust embedding API, `lua::Definition::bind_recognizer`, binds an
approved `input` subscription ID to a declaration. It requires observation mode,
protected normalized-input permission and explicit permitted device selectors.
Names and subscription IDs are unique, with at most 64 bindings. Bindings travel
with the approved complete source snapshot; re-evaluation must reproduce its
configuration and subscription metadata. This is a host API, not the finished
Lua declaration interface. No new Lua constructor is advertised.

After instantiation, the bound subscription receives only ticketed callbacks;
ordinary input dispatch skips it. `Runtime::recognize` requires fresh owning
desktop context, matching fence/seat, negotiated normalized-input capabilities
and the original unexpired ticket. It checks these before execution and after
the callback. Unknown, expired or foreign context produces no proposal without
entering Lua. Repeated/reordered delivery is rejected before changing closure
state. Ticket/candidate/revision/fence/source/contact identities are copied as
exact decimal strings. Editing the copy cannot retarget the native proposal.

The callback returns nil for pending, or one `{phase=..., data=...}` table. Other
fields and yielding fail. Lua receives the existing 20 ms / 100,000-instruction
callback budget; expanded table traversal is bounded before deserialization,
including shared/cyclic tables. Data still obeys the Rust eight-level / 4 KiB
limit. Callback failure or expiry during execution retires the VM, discards its
logs/proposals and is never retried. Structured logging also preflights expanded
tables before native allocation.

`session::Session::request_recognition` and `take_recognitions` implement the
private supervised request/response channel, with fence/sequence/ticket checks,
bounded pending/results queues and normal stop retirement. Missing proposals
mean no recognition. This private transport does not create authenticated public
broker or worker admission. The caller must pass any returned proposal back to
the owning Rust gate; even a validated result still requires real routing
arbitration. The source constructor, authenticated broker/private input-worker
path, current eligibility/winner integration, cancellation cleanup, wake
scheduling, reload and actual routing tests remain required.
