# Continuous viewport motion

G11 / P09.5 is **not enabled**. `ScrollStack` remains excluded from production
action and claim gates. This document describes its numerical foundation, not
an execution API or evidence of rendered gesture delivery.

## Verified native contract

The audited stock version is Hyprland 0.56.2, commit
`efb50993780079460b0cbed1363e2166a2de1d9f`:

- [Numeric layout movement](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/layout/algorithm/tiled/scrolling/ScrollingAlgorithm.cpp)
  uses `controller->adjustOffset(-distance)` followed by recalculation. It moves
  the viewport; `move +col` instead steps between columns. Native focus changes
  after numeric movement are a side effect, not the requested operation.
- [The camera controller](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/layout/algorithm/tiled/scrolling/ScrollTapeController.cpp)
  does not clamp the far edge on numeric movement. Short content is centered at
  `round((content_extent - viewport_extent) / 2)`. Up reverses the primary axis;
  down does not. Layout rules, fullscreen behavior, reserved areas and workspace
  gaps affect the effective geometry.
- [The layout manager](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/layout/LayoutManager.cpp)
  selects the focused monitor's active special workspace or ordinary workspace.
  Numeric movement does not carry an explicit output/workspace target.

Global `scrolling:direction` alone does not establish effective direction:
workspace `layoutopts` can override it. Window goal geometry alone does not
establish rendered side-strip eligibility.

## Numerical planner

`prek::hyprland::scrolling` performs no I/O. `Viewport::new` validates finite,
bounded effective extents and an in-bounds native camera offset. Content shorter
than the viewport has one fixed, rounded centered offset and produces no motion.

`Motion::begin` takes the unique gesture ID, immutable viewport, cumulative
logical `total_y` and monotonic time. Begin is ordinal one and includes initial
recognition travel. Update/end ordinals must increase; gaps from coalescing are
allowed. Destinations are anchored to the initial offset, then clamped to the
content bounds. Down maps to `initial_offset - total_y`, up to
`initial_offset + total_y`. Overshooting a bound does not change the anchor.

`take` retains at most one in-flight attempt and one latest desired destination.
It allows at most one new attempt per 16 ms, including end. An attempt provides
the source/destination offsets and the native numeric `layout_move` distance.
It is **not** a dispatch command, grant, or permission to claim input.

`settle` requires that exact outstanding attempt and an exact observed native
offset. Missing, different or nonfinite observations retire the motion as
uncertain: no retry or later progress is allowed. Repeated/foreign receipts do
not settle another attempt. Cancellation drops pending progress without issuing
a rollback; already queued effects still need settlement. Cancellation is not
proof that an in-flight mutation was retracted.

Run the numerical, typed-dispatch and adapter models with `oslo make test-hyprland`.
They use fake monotonic time and do not open a compositor, touch devices or grab
input. Model success does not establish native or physical acceptance.

## Native layout metadata

`ContextSampler::sample_layouts(client, generation)` queries the verified
Hyprland **0.56.2 Lua** process. Other versions/providers refuse without trying
REPL or dispatch syntax alternatives. It returns a generation/revision/timestamp
and bounded native workspace records: output/monitor identity, active/special
state, tiled algorithm, mapped window lifetime IDs and goal rectangles,
floating/grouped/fullscreen flags, and scrolling column indices, normalized
widths, ordered member lifetimes and each member's row.

The collector sends one fixed `repl` expression, not caller-supplied Lua. It
reads native getters and builds local JSON; it registers no globals, callbacks,
IPC commands, plugins or configuration. Unlike ordinary `eval`, the official
[REPL implementation](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/config/lua/ConfigManager.cpp)
returns a successful scalar string as IPC output. No intentional errors are used
as a data channel. Root does not evaluate user programs.

Lock observations bracket collection and latch inhibition, even if the returned
document is malformed. The actual config provider is checked before and after.
All requests share one 200 ms deadline and retained socket/peer/process checks.
Only successful collection advances the sampler revision. The caller still
needs its verified logind binding and freshness checks; this API grants nothing.

Collection/decoding are bounded to 64 workspaces and 512 mapped windows/column
members. Strict JSON rejects unknown/trailing fields, malformed identities,
duplicate addresses/lifetimes, inconsistent columns/rows, invalid geometry and
ambiguous active workspaces. Full graph membership is checked, not just counts.

**This is not the native camera offset, effective workspace-rule direction,
work area, or rendered geometry.** It does not enable G11 or claim side-strip
input. Those fields still need an authoritative contract and broker/executor
integration. Do not invent missing bounds from the number of visible windows.

The broker's combined sampler now attaches only graphs that match the same
desktop generation/revision, conservative original capture time, complete
mapped-window identities/goals and output/workspace association. Optional native
reads get 50 ms inside the existing total 200 ms budget; failure drops only the
inspection graph unless lock inhibition invalidates all desktop context. The
chunked user-context path exposes fresh copy-only `context.layouts()` data with
lossless identities; see [the Lua reference](lua-api.md#native-layout-inspection).
Graphs are deliberately not private-worker preclaim proof: their new inspection
field is omitted from the private recognition projection, while its complete
core geometry stays unchanged. Native action proof/transport remains a separate
required integration gate before any layout-dependent ownership is enabled.

`oslo make test-hyprland` also runs closed-query Luna/JSON and socket models.
`oslo make test-cage-lua-wayland-fix` exercises the actual read-only query on
three owned windows, changes only the owned test layout down/up, checks column
membership/goal direction/lifetimes, then restores dwindle. Its
`hyprland-layouts.json` is metadata acceptance, **not** continuous G11 rendering
or unmodified stock/native/physical routing acceptance.

## Remaining integration gates

Before enabling G11:

1. Obtain authoritative effective layout/direction, camera bounds and complete
   column/window lifetime evidence. Preserve reserved areas, keyboard insets and
   per-workspace overrides. Do not infer bounds from window count or only the
   visible windows. Native Lua getters are not automatically query replies:
   ordinary `eval` returns `ok`. The fixed REPL collector above obtains native
   columns, not the missing effective camera/work-area fields.
2. Bind that evidence through broker/context to preclaim eligibility for the
   actual rendered empty strip. Wrong/unknown layouts and unsupported backends
   must refuse before ownership, not consume then discover failure.
3. Freeze output/workspace/topology, column order/lifetimes and the actual config
   provider. Guard the focused-workspace native operation atomically; never
   focus a different target to make a global dispatcher apply to it.
4. Wire begin/update/end/cancel, generation loss and bounded queues through the
   session executor with fresh per-attempt grants. A numerical attempt is not a
   grant. Do not retry a relative mutation after an uncertain write/receipt.
5. Prove actual continuously rendered multiwindow movement with order/workspace
   preserved, both bounds, reversal, cancellation, keyboard-visible geometry,
   retarget refusal and ordinary app input. Keep test-only compositor dependency
   fixes separate from unmodified stock/native/physical acceptance.
