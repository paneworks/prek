# Private worker subscriptions

The authenticated supervisor/worker channel carries complete broker-approved
subscription sets independently of configuration snapshots. Public clients
cannot submit these commands or provide desktop authority.

## Bounds and ordering

- At most eight seats and 32 MiB of serialized body data, including all core
  geometry and optional matched native layout graphs. No targets are truncated.
- One transfer in flight plus one coalesced latest refresh at the sender;
  one partial body at the receiver. A normal refresh does not restart a body.
- `subscriptions_begin` contains a positive monotonically increasing revision,
  exact byte count and canonical seat-sorted safety scopes. It must fit the
  ordinary 64 KiB packet limit, including worst-case envelope counters.
- `subscriptions_chunk` contains that revision, exact byte offset and base64
  data. Chunks contain 8192 decoded bytes except the exact final remainder.
- `subscriptions_commit` activates only a complete strictly decoded body with
  the same revision and identical scope set. There is no partial activation.
- Each send call offers at most four packets; a full socket leaves unsent state
  unchanged. Workers read at most eight commands per service pass. Input and
  supervision remain runnable between bursts.

## Revocation barrier

Scopes bind seat, registration fence, consumer, capabilities and the context's
binding/generation/security state. They contain no geometry or capture-time
renewal. A new header removes absent or changed registrations immediately and
intersects retained capabilities. It cannot activate a seat, add a capability,
renew a registration/provider lease or replace old geometry. Binding or context
presence changes require a fresh fence. Security changes preempt an in-flight
sender transfer; ordinary geometry refreshes coalesce instead.

Partial bodies expire after one second. Workers discard them and report context
loss without stopping ordinary input processing. Successful configuration
installation discards partial subscription data. Late chunks/commits from
retired revisions are ignored; regressing headers, active offsets, malformed
bodies and unauthenticated packets are not accepted. Legacy direct updates share
the same revision domain and supersede partial bodies.

Receivers use the actual monotonic time after decoding the body to validate
original capture freshness. Queueing, decoding, promoting a coalesced refresh or
receiving a header never refreshes the capture timestamp. Native metadata remains
inspection data, not camera bounds, rendered input scope or an execution grant.

Run `oslo make test-subscription-transfer` for actual-channel, pressure, whole
graph, privacy barrier, expiry and both worker integration cases. Isolated
routing fixtures do not open kernel devices or establish physical acceptance.
