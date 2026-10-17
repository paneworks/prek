# Vertical app reordering

`reorder_up` and `reorder_down` are experimental Hyprland actions for the exact
0.56.2 Lua provider and its native scrolling layout. Production input takeover
remains disabled. Source/ownership, rendered app-content eligibility, physical
phone behavior and the complete G08 acceptance matrix remain separate gates.

## Frozen selection

The broker and routing worker require a complete native layout graph matching
the desktop sample's generation, revision, capture time and mapped windows.
An ordinary context update without that graph clears native eligibility.
Missing/stale context, locked/secure/off outputs, unsupported providers and
unknown rendered app-content visibility do not authorize consumption.

Up/down follows the vertical order of the native column goal boxes, including
the reversed scrolling direction. The adjacent column must be unambiguous and
nonoverlapping vertically. Within it, select the uniquely closest horizontally
overlapping app by center alignment. Equal alignment is unavailable, not a
choice based on map iteration or current focus. At a boundary there is no action.
The source and neighbor must be ordinary nonhidden, nonfloating, nongrouped,
nonfullscreen tiled apps. Horizontal/overlapping columns refuse this operation.

This selection does not compute a scrolling camera offset, effective work area
or rendered empty strip. It does not enable G11 viewport movement.

## Effect lifecycle

Before input claim, the routing worker freezes a bounded SHA-256 fingerprint
of the source, output and complete canonical workspace graph. The private
dispatch, broker offer and separate execution grant carry the same proof.
The combined sampler stamps a native continuity incarnation, preserved only
across unchanged fresh desktop/layout samples. Graph loss, state changes or a
freshness gap advance it even if an identical graph later returns. Unbound
inspection graphs have no reorder incarnation. Delayed private-channel claims
therefore cannot be rebound to a later identical desktop before their first
broker offer. Queued worker reports also retire native permission immediately;
generic named handlers retain only their independent action families.

Sample-revision refreshes and focus changes preserve the proof; graph, provider,
capability or freshness loss permanently retires it for that gesture. Restoring
the original graph does not restore an old permission. The proof is trusted
correlation metadata, not an independent capability or a Lua-configurable value.

The session offer freezes both possible adjacent targets and their graph.
A later handler may choose only an eligible frozen direction, never a newly
arrived neighbor. Missing or mismatched offer/grant proofs disable reorder.
Retiring this native permission does not disable an otherwise valid named
handler's close/move action. The complete returned plan is checked before any
observation, command or native effect. Protected
`policy.allow_hyprland_actions`, the owning agent's capability and a fresh
separate broker grant remain required.

The executor validates another native sample under the remaining grant and
shared 200 ms IPC deadline. The final native Lua factory then checks the source
window/output guards, workspace identity, mapped membership, every window
lifetime/goal/state/column position, and every column width/member order again
before constructing one explicit-source/explicit-neighbor `window.swap`.
It does not focus another window to select an action target. The compositor's
normal cursor/camera side effects are not substituted for the actual swap.

Applied requires observing exactly that pair permutation while preserving the
workspace, column topology, other membership and native app states. Goal boxes
may move with the native camera after the swap; they are not invented or used
as camera authority. Read-only receipt sampling cannot retry the mutation.
Lost/malformed acknowledgements, changed state or missing confirmation after
bytes were sent remain uncertain.

## Verification

`oslo make test-hyprland` includes selection, multi-member ambiguity, the
whole-graph native factory, preclaim retirement, private-worker transport,
broker grants and socket/executor models. Missing/changed proofs, provider
loss, neighbor replacement, restored graphs and lost acknowledgements exercise
the same single-attempt path. Real native Luna helpers also check that a named
handler can still close while a retired reorder plan emits no partial effects.
These models open no compositor or input devices.

The owned Lua Cage fixture exercises named Lua handlers and model grants in
both native scrolling directions, swaps the same app up and back down, checks
the actual native pair permutation and saves `reorder-{down,up}-*.png` with
named receiver labels. Before capture it requires matching configure, frame
and actual `wp_presentation` feedback for each expected visible surface. Real
PPM pixels must contain each receiver's RGB marker at its native position;
the PNG and rendering receipts are retained alongside the native permutation.
A black frame observed during development was rejected by this check; the
presentation-backed development fixture passed both native directions.

The GUI receipts explicitly mark modeled claims/grants and absent physical
takeover. They do not establish native broker, physical app-content or full G08
acceptance. Unmodified stock/native broker and physical acceptance are not
implied by a test-only nested-compositor dependency fix.
