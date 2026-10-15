---@meta prek
-- Editor definitions for the embedded Prek host, not a standalone runtime.
-- Add this directory to Lua.workspace.library; never copy it into modules/.
-- Rust validation and broker permissions remain authoritative.

---@alias prek.Identity string Exact unsigned 64-bit decimal identity; compare strings, never tonumber.
---@alias prek.Key string Linux key name/alias or code:N.
---@alias prek.Duration string Whole-number ms, s or m, e.g. "250ms".
---@alias prek.Distance string Explicit mm, norm or px, e.g. "3mm" or "0.1norm".
---@alias prek.Mode 'observe'|'route'
---@alias prek.Delivery 'observe'|'buffer'|'reserve'
---@alias prek.State 'console'|'desktop'|'locking'|'locked'|'greeter'|'inactive'|'unknown'
---@alias prek.Context 'console'|'desktop'|'locking'|'locked'|'greeter'
---@alias prek.Scope 'everywhere'|'app_content'|'keyboard'|'content_bottom'|'physical_bottom'|'top_left'|'top_right'|'side_left'|'side_right'|'clock_background'
---@alias prek.Direction 'up'|'down'|'left'|'right'|'horizontal'|'vertical'|'any'
---@alias prek.Screen 'on'|'off'
---@alias prek.LayerActivation 'physical'|'active'|'inactive'
---@alias prek.Stage 'discrete'|'begin'|'update'|'end'|'cancel'
---@alias prek.TaskState 'pending'|'running'|'sleeping'|'completed'|'cancelled'
---@alias prek.EffectKind 'observations'|'commands'|'synthetic_input'|'hyprland_actions'|'morf_actions'|'power_actions'
---@alias prek.EventKind 'input'|'matched'|'begin'|'update'|'end'|'cancel'|'skipped'|'ready'|'reload'|'stop'|'recovery'|'session_changed'|'context_changed'|'rules_changed'
---@alias prek.MorfAction 'dashboard'|'notifications'|'quick_settings'|'workspace_preview'|'keyboard_show'|'keyboard_hide'|'keyboard_toggle_layout'|'apps'|'web'|'reveal_clock_form'
---@alias prek.WindowAction 'reorder_up'|'reorder_down'|'move_left_and_follow'|'move_right_and_follow'|'close'|'scroll_stack'
---@alias prek.PowerAction 'lock_and_blank'|'wake'|'blank'

---@class (exact) prek.Timings
---@field tap_max? prek.Duration Default 250ms.
---@field tap_gap? prek.Duration Default 300ms.
---@field hold_after? prek.Duration Default 600ms.
---@field join_window? prek.Duration Default 120ms.
---@field decision_window? prek.Duration Default 300ms.
---@field swipe_timeout? prek.Duration Default 1500ms.
---@field tap_motion? prek.Distance Default 3mm.
---@field fallback_motion? prek.Distance Default 0.01norm.
---@field pan_slop? prek.Distance Default 3mm.

---@class (exact) prek.Selector
---@field identity? string
---@field path? string
---@field name? string
---@field physical? string
---@field unique? string
---@field seat? string
---@field keys? prek.Key[] Required capabilities, not held-key conditions.
---@field absolute_axes? integer[]
---@field properties? integer[]

---@class (exact) prek.Policy
---@field allowed_devices? string[]
---@field allowed_seats? string[]
---@field allow_runtime_rules? boolean Default false; only protected system policy grants permission.
---@field allow_input_events? boolean Default false; requires protected policy, --lua --input and fresh desktop context.

---@class (exact) prek.ShapeCapture
---@field max_samples? integer Default 2048.
---@field timeout? prek.Duration Default 5s.
---@field minimum_length? prek.Distance Default 20mm.
---@field minimum_size? prek.Distance Default 12mm.
---@field fallback_length? prek.Distance Default 0.1norm.
---@field fallback_size? prek.Distance Default 0.04norm.
---@field mouse_minimum_length_counts? number Default 20.
---@field mouse_minimum_size_counts? number Default 12.

---@class (exact) prek.ShapeTemplate
---@field name string
---@field points [number, number][]
---@field threshold number
---@field margin number
---@field rotation_invariant? boolean Default false.
---@field reflection_invariant? boolean Default false.

---@class (exact) prek.Layer
---@field id string Nonempty, at most 256 bytes, no control characters.
---@field key prek.Key Physical binding is required even with an activation override.
---@field toggle? boolean Default false.
---@field activation? prek.LayerActivation Default physical; this is an override, not physical key state.

---@class (exact) prek.Repeat
---@field delay prek.Duration
---@field interval prek.Duration

---@class (exact) prek.KeyFields
---@field key prek.Key
---@class (exact) prek.KeyTriggerFields: prek.KeyFields
---@field kind? 'key'
---@class (exact) prek.RemapFields: prek.KeyFields
---@field kind? 'remap'
---@class (exact) prek.ButtonTapsFields: prek.KeyFields
---@field kind? 'button_taps' Matching explicit kind is accepted; constructors supply it if absent.
---@field count integer
---@class (exact) prek.ButtonHoldFields: prek.KeyFields
---@field kind? 'button_hold' Matching explicit kind is accepted; constructors supply it if absent.
---@field after prek.Duration
---@field repeat? prek.Repeat
---@class (exact) prek.ChordFields
---@field kind? 'chord' Matching explicit kind is accepted; constructors supply it if absent.
---@field keys prek.Key[]
---@field ordered boolean
---@field timeout prek.Duration
---@class (exact) prek.WheelFields
---@field kind? 'wheel' Matching explicit kind is accepted; constructors supply it if absent.
---@field horizontal boolean
---@field minimum number
---@class (exact) prek.TouchTapsFields
---@field kind? 'touch_taps' Matching explicit kind is accepted; constructors supply it if absent.
---@field fingers integer
---@field count integer
---@class (exact) prek.TouchHoldFields
---@field kind? 'touch_hold' Matching explicit kind is accepted; constructors supply it if absent.
---@field fingers integer
---@field after prek.Duration
---@class (exact) prek.SwipeFields
---@field kind? 'swipe' Matching explicit kind is accepted; constructors supply it if absent.
---@field fingers integer
---@field direction prek.Direction
---@field travel prek.Distance
---@field fallback_travel prek.Distance
---@field continuous? boolean Default false; pan forces true.
---@class (exact) prek.PanFields: prek.SwipeFields
---@field continuous? true Pan rejects explicit false.
---@class (exact) prek.PinchFields
---@field kind? 'pinch' Matching explicit kind is accepted; constructors supply it if absent.
---@field fingers integer
---@field ratio number
---@class (exact) prek.RotateFields
---@field kind? 'rotate' Matching explicit kind is accepted; constructors supply it if absent.
---@field fingers integer
---@field degrees number
---@class (exact) prek.ShapeFields
---@field kind? 'shape' Matching explicit kind is accepted; constructors supply it if absent.
---@field name string
---@field fingers? integer Absent selects a mouse path.
---@field button? prek.Key Mouse capture delimiter, default BTN_LEFT.
---@class (exact) prek.SequenceFields
---@field kind? 'sequence' Matching explicit kind is accepted; constructors supply it if absent.
---@field steps prek.Trigger[]
---@field timeout prek.Duration

---@class (exact) prek.KeyTrigger: prek.KeyTriggerFields
---@field kind 'key'
---@class (exact) prek.ButtonTapsTrigger: prek.ButtonTapsFields
---@field kind 'button_taps'
---@class (exact) prek.ButtonHoldTrigger: prek.ButtonHoldFields
---@field kind 'button_hold'
---@class (exact) prek.ChordTrigger: prek.ChordFields
---@field kind 'chord'
---@class (exact) prek.WheelTrigger: prek.WheelFields
---@field kind 'wheel'
---@class (exact) prek.TouchTapsTrigger: prek.TouchTapsFields
---@field kind 'touch_taps'
---@class (exact) prek.TouchHoldTrigger: prek.TouchHoldFields
---@field kind 'touch_hold'
---@class (exact) prek.SwipeTrigger: prek.SwipeFields
---@field kind 'swipe'
---@class (exact) prek.PinchTrigger: prek.PinchFields
---@field kind 'pinch'
---@class (exact) prek.RotateTrigger: prek.RotateFields
---@field kind 'rotate'
---@class (exact) prek.ShapeTrigger: prek.ShapeFields
---@field kind 'shape'
---@class (exact) prek.SequenceTrigger: prek.SequenceFields
---@field kind 'sequence'
---@alias prek.Trigger prek.KeyTrigger|prek.ButtonTapsTrigger|prek.ButtonHoldTrigger|prek.ChordTrigger|prek.WheelTrigger|prek.TouchTapsTrigger|prek.TouchHoldTrigger|prek.SwipeTrigger|prek.PinchTrigger|prek.RotateTrigger|prek.ShapeTrigger|prek.SequenceTrigger

---@class (exact) prek.EventFields
---@field kind? 'event' Matching explicit kind is accepted; constructors supply it if absent.
---@field name string
---@class (exact) prek.KeysFields
---@field kind? 'keys' Matching explicit kind is accepted; constructors supply it if absent.
---@field keys prek.Key[]
---@class (exact) prek.CommandFields
---@field kind? 'command' Matching explicit kind is accepted; constructors supply it if absent.
---@field argv string[] Exact argv, no implicit shell; at most 256 arguments, 4096 bytes each.
---@class (exact) prek.MorfFields
---@field kind? 'morf' Matching explicit kind is accepted; constructors supply it if absent.
---@field action prek.MorfAction
---@class (exact) prek.HyprlandFields
---@field kind? 'hyprland' Matching explicit kind is accepted; constructors supply it if absent.
---@field action prek.WindowAction
---@class (exact) prek.PowerFields
---@field kind? 'power' Matching explicit kind is accepted; constructors supply it if absent.
---@field action prek.PowerAction
---@class (exact) prek.HandlerOptions
---@field requires prek.EffectKind[] Nonempty declared effect capabilities.
---@class (exact) prek.EventAction: prek.EventFields
---@field kind 'event'
---@class (exact) prek.KeysAction: prek.KeysFields
---@field kind 'keys'
---@class (exact) prek.RemapAction: prek.RemapFields
---@field kind 'remap'
---@class (exact) prek.CommandAction: prek.CommandFields
---@field kind 'command'
---@class (exact) prek.MorfActionDeclaration: prek.MorfFields
---@field kind 'morf'
---@class (exact) prek.HyprlandAction: prek.HyprlandFields
---@field kind 'hyprland'
---@class (exact) prek.PowerActionDeclaration: prek.PowerFields
---@field kind 'power'
---@class (exact) prek.LuaAction: prek.HandlerOptions
---@field kind 'lua'
---@field handler string Approved named handler in this exact source generation.
---@alias prek.Action prek.EventAction|prek.KeysAction|prek.RemapAction|prek.CommandAction|prek.MorfActionDeclaration|prek.HyprlandAction|prek.PowerActionDeclaration|prek.LuaAction
---@alias prek.ActionResult prek.EventAction|prek.CommandAction Only these named-handler effects are currently executable.

---@class (exact) prek.Rule
---@field id string
---@field enabled? boolean Default true.
---@field priority? integer Default 0; equally prioritized overlapping exclusive rules are rejected.
---@field devices? string[] Selector names.
---@field seats? string[]
---@field contexts? prek.Context[]
---@field scope? prek.Scope Default everywhere.
---@field screen? prek.Screen
---@field keyboard_visible? boolean
---@field require_keys? prek.Key[]
---@field keys_throughout? boolean Default false.
---@field layer? string Declared layer ID.
---@field delivery? prek.Delivery Default observe.
---@field trigger prek.Trigger
---@field action prek.Action

---@class (exact) prek.Config
---@field version? integer config.new supplies 1; all other defaults apply during Rust validation.
---@field mode? prek.Mode Default observe. Production route mode remains unavailable.
---@field timing? prek.Timings
---@field shape_capture? prek.ShapeCapture
---@field policy? prek.Policy
---@field devices? table<string, prek.Selector>
---@field layers? prek.Layer[] At most 64 declarations.
---@field shapes? prek.ShapeTemplate[]
---@field rules? prek.Rule[]
---@class (exact) prek.Preset: prek.Config
---@field version integer
---@field mode prek.Mode
---@field timing prek.Timings
---@field shape_capture prek.ShapeCapture
---@field policy prek.Policy
---@field devices table<string, prek.Selector>
---@field layers prek.Layer[]
---@field shapes prek.ShapeTemplate[]
---@field rules prek.Rule[]

---@class (exact) prek.MonotonicTime
---@field secs integer
---@field nanos integer
---@class (exact) prek.Fence
---@field broker [prek.Identity, prek.Identity]
---@field configuration prek.Identity
---@field session prek.Identity
---@class (exact) prek.Source
---@field device prek.Identity
---@field generation prek.Identity
---@field seat string
---@class (exact) prek.Point
---@field x number
---@field y number
---@class (exact) prek.Rect
---@field origin prek.Point
---@field size prek.Point
---@class (exact) prek.Output
---@field name string
---@field pixel_width number
---@field pixel_height number
---@field scale number
---@field origin prek.Point
---@field transform 'normal'|'rotate90'|'rotate180'|'rotate270'|'flipped'|'flipped90'|'flipped180'|'flipped270'
---@class (exact) prek.Keyboard
---@field rect prek.Rect
---@field layout string
---@class (exact) prek.OutputState
---@field geometry prek.Output
---@field geometry_generation prek.Identity
---@field active_workspace? integer
---@field screen prek.Screen
---@field keyboard? prek.Keyboard
---@class (exact) prek.Window
---@field id string
---@field output string
---@field workspace integer
---@field rect prek.Rect
---@field content prek.Rect
---@field mapped boolean
---@field visible boolean
---@field focused boolean
---@field z_order? integer
---@class (exact) prek.Region
---@field id string
---@field scope? prek.Scope
---@field output string
---@field rect prek.Rect
---@field interactive boolean
---@class (exact) prek.Snapshot
---@field generation prek.Identity
---@field revision prek.Identity
---@field captured_at prek.MonotonicTime
---@field state prek.State
---@field outputs prek.OutputState[]
---@field windows prek.Window[]
---@field regions prek.Region[]
---@field secure_overlay boolean
---@class (exact) prek.Binding
---@field seat string
---@field session string
---@field uid integer
---@field compositor_pid integer
---@field compositor_start prek.Identity
---@field kind 'user'|'greeter'

---@alias prek.Space 'normalized'|'logical'|'millimetres'|'relative'
---@alias prek.CancelReason 'event_loss'|'backward_time'|'invalid_frame'|'source_changed'|'disconnected'|'mapping_changed'
---@class (exact) prek.TouchTaps
---@field kind 'taps'
---@field fingers integer
---@field count integer
---@field position prek.Point
---@class (exact) prek.TouchHold
---@field kind 'hold'
---@field fingers integer
---@field position prek.Point
---@class (exact) prek.TouchPan
---@field kind 'pan'
---@field fingers integer
---@field phase 'begin'|'update'|'end'
---@field direction prek.Direction
---@field total prek.Point
---@field delta prek.Point
---@field velocity prek.Point
---@field centroid prek.Point
---@field swipe boolean
---@class (exact) prek.TouchPinch
---@field kind 'pinch'
---@field fingers integer
---@field phase 'begin'|'update'|'end'
---@field ratio number
---@class (exact) prek.TouchRotate
---@field kind 'rotate'
---@field fingers integer
---@field phase 'begin'|'update'|'end'
---@field degrees number
---@class (exact) prek.TouchCancelled
---@field kind 'cancelled'
---@field reason prek.CancelReason
---@alias prek.TouchGesture prek.TouchTaps|prek.TouchHold|prek.TouchPan|prek.TouchPinch|prek.TouchRotate|prek.TouchCancelled
---@class (exact) prek.KeyEvidence
---@field kind 'key'
---@field down boolean
---@class (exact) prek.TapsEvidence
---@field kind 'taps'
---@field count integer
---@class (exact) prek.HoldEvidence
---@field kind 'hold'
---@field repeat boolean
---@class (exact) prek.ChordEvidence
---@field kind 'chord'
---@class (exact) prek.TouchEvidence
---@field kind 'touch'
---@field space prek.Space
---@field gesture prek.TouchGesture
---@class (exact) prek.WheelEvidence
---@field kind 'wheel'
---@field horizontal boolean
---@field amount number
---@class (exact) prek.ShapeMatch
---@field name string
---@field confidence number
---@field margin number
---@class (exact) prek.ShapeEvidence
---@field kind 'shape'
---@field space prek.Space
---@field fingers? integer
---@field shape prek.ShapeMatch
---@class (exact) prek.SequenceEvidence
---@field kind 'sequence'
---@field steps prek.Evidence[]
---@alias prek.Evidence prek.KeyEvidence|prek.TapsEvidence|prek.HoldEvidence|prek.ChordEvidence|prek.TouchEvidence|prek.WheelEvidence|prek.ShapeEvidence|prek.SequenceEvidence

---@class (exact) prek.MatchedEvent
---@field outcome 'matched'
---@field version integer
---@field rule string
---@field id prek.Identity
---@field ordinal prek.Identity
---@field source prek.Source
---@field at prek.MonotonicTime
---@field config_generation prek.Identity
---@field session_generation prek.Identity
---@field stage prek.Stage
---@field action prek.Action
---@field evidence prek.Evidence Narrow on evidence.kind before accessing variant fields.
---@field window? string
---@field output? string
---@field workspace? integer
---@class (exact) prek.SkippedEvent
---@field outcome 'skipped'
---@field version integer
---@field rule string
---@field source prek.Source
---@field at prek.MonotonicTime
---@field reason string
---@class (exact) prek.CancelledEvent
---@field outcome 'cancelled'
---@field version integer
---@field rule string
---@field id prek.Identity
---@field source prek.Source
---@field at prek.MonotonicTime
---@field reason string
---@class (exact) prek.FencedPayload
---@field fence prek.Fence
---@class (exact) prek.ContactId
---@field slot integer
---@field tracking integer
---@field generation prek.Identity
---@class (exact) prek.Contact
---@field id prek.ContactId
---@field x integer Device axis counts.
---@field y integer Device axis counts.
---@field pressure? integer
---@field fresh boolean False for contacts already held at reconciliation.
---@class (exact) prek.KeyChange
---@field kind 'key'
---@field code integer Linux evdev code, without text/layout translation.
---@field state 'press'|'release'|'repeat'
---@class (exact) prek.RelativeChange
---@field kind 'relative'
---@field code integer
---@field value integer Device counts, without a second acceleration curve.
---@class (exact) prek.AbsoluteChange
---@field kind 'absolute'
---@field code integer
---@field value integer Device axis counts.
---@class (exact) prek.SwitchChange
---@field kind 'switch'
---@field code integer
---@field on boolean
---@class (exact) prek.WheelChange
---@field kind 'wheel'
---@field horizontal boolean
---@field steps number
---@field high_resolution boolean
---@alias prek.InputChange prek.KeyChange|prek.RelativeChange|prek.AbsoluteChange|prek.SwitchChange|prek.WheelChange
---@class (exact) prek.InputFrame
---@field kind 'frame'
---@field source prek.Source
---@field at prek.MonotonicTime
---@field sequence prek.Identity
---@field selectors string[] Matched protected allowlist selector names.
---@field changes prek.InputChange[]
---@field contacts prek.Contact[]
---@field contacts_started prek.ContactId[]
---@field contacts_ended prek.ContactId[]
---@field ended_contacts prek.Contact[] Last positions, including motion in a release frame.
---@class (exact) prek.InputReset
---@field kind 'reset'
---@field source prek.Source
---@field at prek.MonotonicTime
---@field selectors string[]
---@field reason 'attached'|'reconciled'|'mapping_changed'|'removed'|'lost'
---@field keys integer[] Already held evdev codes; no invented press events.
---@field contacts prek.Contact[] Already held contacts have fresh=false. Removal/loss clears state.
---@class (exact) prek.InputPayload: prek.FencedPayload
---@field input prek.InputFrame|prek.InputReset
---@class (exact) prek.MatchedPayload: prek.FencedPayload
---@field event prek.MatchedEvent
---@class (exact) prek.SkippedPayload: prek.FencedPayload
---@field event prek.SkippedEvent
---@class (exact) prek.CancelPayload: prek.FencedPayload
---@field event prek.CancelledEvent|prek.MatchedEvent
---@class (exact) prek.ReadyPayload: prek.FencedPayload
---@field seat string
---@class (exact) prek.SessionPayload: prek.FencedPayload
---@field state prek.State
---@class (exact) prek.ContextChangedPayload: prek.SessionPayload
---@field available boolean
---@field revision? prek.Identity
---@class (exact) prek.RulesChangedPayload: prek.FencedPayload
---@field accepted boolean
---@field revision prek.Identity
---@field error? string
---@alias prek.EventPayload prek.InputPayload|prek.MatchedPayload|prek.SkippedPayload|prek.CancelPayload|prek.ReadyPayload|prek.SessionPayload|prek.ContextChangedPayload|prek.RulesChangedPayload|prek.FencedPayload
---@class (exact) prek.TimerWake
---@field context prek.FencedPayload
---@field timer { id: integer, periodic: boolean }
---@field at prek.MonotonicTime
---@class (exact) prek.TaskWake
---@field context prek.FencedPayload
---@field task { id: integer }
---@field at prek.MonotonicTime

---@class prek.Subscription
local Subscription = {}
---Idempotently disable future invocations in this VM generation.
function Subscription:dispose() error("Prek editor definition") end

---@class prek.Timer
local Timer = {}
---Idempotently cancel this generation-scoped timer.
function Timer:cancel() error("Prek editor definition") end

---@class prek.Task
local Task = {}
---Cancel future resumes. Self-cancellation discards new unsent proposals.
function Task:cancel() error("Prek editor definition") end
---@return prek.TaskState
function Task:status() error("Prek editor definition") end

---@class (exact) prek.RuleTransactionStatus
---@field state 'open'|'cancelled'|'pending'|'accepted'|'rejected'
---@field revision? prek.Identity
---@field error? string
---@class prek.RuleTransaction
local RuleTransaction = {}
---@param id string
function RuleTransaction:enable(id) error("Prek editor definition") end
---@param id string
function RuleTransaction:disable(id) error("Prek editor definition") end
---Stage replacement/addition; validate the complete batch before submission.
---@param rule prek.Rule
function RuleTransaction:replace(rule) error("Prek editor definition") end
---@param id string
function RuleTransaction:remove(id) error("Prek editor definition") end
---@param id string
function RuleTransaction:activate_layer(id) error("Prek editor definition") end
---@param id string
function RuleTransaction:deactivate_layer(id) error("Prek editor definition") end
---@param id string
function RuleTransaction:reset_layer(id) error("Prek editor definition") end
---Submit 1–64 operations, at most 32 KiB. Queries change only after acknowledgement.
---Requires protected allow_runtime_rules policy, --lua --rule-mutations and owning desktop context.
---At most one pending proposal per generation; host receipt deadline 250ms.
function RuleTransaction:commit() error("Prek editor definition") end
---Only open builders can be abandoned; a submitted proposal awaits its receipt.
function RuleTransaction:cancel() error("Prek editor definition") end
---@return prek.RuleTransactionStatus
function RuleTransaction:status() error("Prek editor definition") end

---@class prek.ConfigAPI
local ConfigAPI = {}
---@class prek.RulesAPI
local RulesAPI = {}
---@class prek.LayersAPI
local LayersAPI = {}
---@class prek.TriggersAPI
local TriggersAPI = {}
---@class prek.ActionsAPI
local ActionsAPI = {}
---@class prek.UnitsAPI
local UnitsAPI = {}
---@class prek.EventsAPI
local EventsAPI = {}
---@class prek.LogAPI
local LogAPI = {}
---@class prek.SchedulerAPI
local SchedulerAPI = {}
---@class prek.ContextAPI
local ContextAPI = {}

---@class (exact) prek.Module
---@field api_version integer
---@field version string
---@field api table<string, unknown> Implemented manifest, including limits and partial runtime status.
---@field config prek.ConfigAPI
---@field rules prek.RulesAPI
---@field layers prek.LayersAPI
---@field triggers prek.TriggersAPI
---@field actions prek.ActionsAPI
---@field units prek.UnitsAPI
---@field events prek.EventsAPI
---@field log prek.LogAPI
---@field scheduler prek.SchedulerAPI
---@field context prek.ContextAPI
local prek = {
    api_version = 1, version = "0.1.0", api = {},
    config = ConfigAPI, rules = RulesAPI, layers = LayersAPI, triggers = TriggersAPI, actions = ActionsAPI,
    units = UnitsAPI, events = EventsAPI, log = LogAPI, scheduler = SchedulerAPI, context = ContextAPI,
}

---Supply version=1 if absent; validate the final model in Rust. No effects or I/O.
---@param fields prek.Config
---@return prek.Config
function ConfigAPI.new(fields) error("Prek editor definition") end
---@param name 'minimal'|'desktop'|'phone'
---@return prek.Preset
function ConfigAPI.preset(name) error("Prek editor definition") end
---Export a fully validated declarative model, bounded to 1 MiB. Named handlers require original source.
---@param config prek.Config
---@return string
function ConfigAPI.export(config) error("Prek editor definition") end
---@param fields prek.Rule
---@return prek.Rule
function RulesAPI.define(fields) error("Prek editor definition") end
---Runtime callback/task only. Return owned acknowledged rules; edits to copies have no effect.
---@return prek.Rule[]
function RulesAPI.list() error("Prek editor definition") end
---@param id string
---@return prek.Rule?
function RulesAPI.get(id) error("Prek editor definition") end
---@return prek.Identity
function RulesAPI.revision() error("Prek editor definition") end
---Runtime callback/task only. Permission failures raise errors; use pcall when optional.
---@return prek.RuleTransaction
function RulesAPI.transaction() error("Prek editor definition") end
---@param fields prek.Layer
---@return prek.Layer
function LayersAPI.define(fields) error("Prek editor definition") end
---Runtime callback/task only. Definitions and activation overrides, not physical held state.
---@return prek.Layer[]
function LayersAPI.list() error("Prek editor definition") end
---@param id string
---@return prek.Layer?
function LayersAPI.get(id) error("Prek editor definition") end
---@return prek.Identity
function LayersAPI.revision() error("Prek editor definition") end
---Shares rules.transaction permissions, revision, builder limit and pending slot.
---@return prek.RuleTransaction
function LayersAPI.transaction() error("Prek editor definition") end
---@param value string
---@return prek.Duration
function UnitsAPI.duration(value) error("Prek editor definition") end
---@param value string
---@return prek.Distance
function UnitsAPI.distance(value) error("Prek editor definition") end

---Generation-scoped subscriptions; at most 256 over this VM's lifetime. No raw input.
---@overload fun(kind: 'input', callback: fun(payload: prek.InputPayload)): prek.Subscription
---@overload fun(kind: 'matched'|'begin'|'update'|'end', callback: fun(payload: prek.MatchedPayload)): prek.Subscription
---@overload fun(kind: 'cancel', callback: fun(payload: prek.CancelPayload)): prek.Subscription
---@overload fun(kind: 'skipped', callback: fun(payload: prek.SkippedPayload)): prek.Subscription
---@overload fun(kind: 'ready', callback: fun(payload: prek.ReadyPayload)): prek.Subscription
---@overload fun(kind: 'reload'|'stop', callback: fun(payload: prek.FencedPayload)): prek.Subscription
---@overload fun(kind: 'recovery'|'session_changed', callback: fun(payload: prek.SessionPayload)): prek.Subscription
---@overload fun(kind: 'context_changed', callback: fun(payload: prek.ContextChangedPayload)): prek.Subscription
---@overload fun(kind: 'rules_changed', callback: fun(payload: prek.RulesChangedPayload)): prek.Subscription
---@param kind prek.EventKind
---@param callback fun(payload: prek.EventPayload)
---@return prek.Subscription
function EventsAPI.on(kind, callback) error("Prek editor definition") end
---Runtime callback/task only. At most 32 records / 16 KiB per dispatch.
---@param message string
---@param fields? table<string, unknown>
function LogAPI.info(message, fields) error("Prek editor definition") end
---Monotonic delay from 1ms through 1h; at most 256 timers per VM lifetime.
---@param duration prek.Duration
---@param callback fun(payload: prek.TimerWake)
---@return prek.Timer
function SchedulerAPI.after(duration, callback) error("Prek editor definition") end
---Preserve phase, skip missed ticks. No catch-up bursts; at most 32 due timers per wake.
---@param period prek.Duration
---@param callback fun(payload: prek.TimerWake)
---@return prek.Timer
function SchedulerAPI.every(period, callback) error("Prek editor definition") end
---@param callback fun(payload: prek.TimerWake)
---@return prek.Timer
function SchedulerAPI.defer(callback) error("Prek editor definition") end
---Start on a later wake. At most 64 active tasks, eight resumes per wake, 20ms/100k instructions per resume.
---@param callback fun(payload: prek.TaskWake)
---@return prek.Task
function SchedulerAPI.spawn(callback) error("Prek editor definition") end
---Task only. Yield until a monotonic deadline, from 1ms through 1h; resume with the current fence.
---@param duration prek.Duration
---@return prek.TaskWake
function SchedulerAPI.sleep(duration) error("Prek editor definition") end

---Owned snapshot copy, fresh for at most 250ms. Nil when unavailable/expired; conveys no authority.
---@return prek.Snapshot?
function ContextAPI.snapshot() error("Prek editor definition") end
---@return prek.Binding?
function ContextAPI.binding() error("Prek editor definition") end
---Unknown after the current view expires.
---@return prek.State
function ContextAPI.state() error("Prek editor definition") end
---@return prek.Fence?
function ContextAPI.fence() error("Prek editor definition") end
---@return string?
function ContextAPI.error() error("Prek editor definition") end

---@param fields prek.KeyTriggerFields
---@return prek.KeyTrigger
function TriggersAPI.key(fields) error("Prek editor definition") end
---@param fields prek.ButtonTapsFields
---@return prek.ButtonTapsTrigger
function TriggersAPI.button_taps(fields) error("Prek editor definition") end
---@param fields prek.ButtonHoldFields
---@return prek.ButtonHoldTrigger
function TriggersAPI.button_hold(fields) error("Prek editor definition") end
---@param fields prek.ChordFields
---@return prek.ChordTrigger
function TriggersAPI.chord(fields) error("Prek editor definition") end
---@param fields prek.WheelFields
---@return prek.WheelTrigger
function TriggersAPI.wheel(fields) error("Prek editor definition") end
---@param fields prek.TouchTapsFields
---@return prek.TouchTapsTrigger
function TriggersAPI.touch_taps(fields) error("Prek editor definition") end
---@param fields prek.TouchHoldFields
---@return prek.TouchHoldTrigger
function TriggersAPI.touch_hold(fields) error("Prek editor definition") end
---@param fields prek.SwipeFields
---@return prek.SwipeTrigger
function TriggersAPI.swipe(fields) error("Prek editor definition") end
---Continuous swipe representation; forces continuous=true.
---@param fields prek.PanFields
---@return prek.SwipeTrigger
function TriggersAPI.pan(fields) error("Prek editor definition") end
---@param fields prek.PinchFields
---@return prek.PinchTrigger
function TriggersAPI.pinch(fields) error("Prek editor definition") end
---@param fields prek.RotateFields
---@return prek.RotateTrigger
function TriggersAPI.rotate(fields) error("Prek editor definition") end
---@param fields prek.ShapeFields
---@return prek.ShapeTrigger
function TriggersAPI.shape(fields) error("Prek editor definition") end
---@param fields prek.SequenceFields
---@return prek.SequenceTrigger
function TriggersAPI.sequence(fields) error("Prek editor definition") end

---Configuration-time only. At most 256 approved handlers, at most 32 effects / 32 KiB per result.
---Runs after broker grant in the unprivileged session; --lua --commands required for executable plans.
---@param name string 1–128 ASCII letters/digits, underscore, hyphen, dot or colon; unique in generation.
---@param options prek.HandlerOptions
---@param callback fun(payload: prek.MatchedPayload): (prek.ActionResult|prek.ActionResult[]|nil)
---@return prek.LuaAction
function ActionsAPI.define(name, options, callback) error("Prek editor definition") end
---Declare an observation effect; evaluation executes no action.
---@param fields prek.EventFields
---@return prek.EventAction
function ActionsAPI.event(fields) error("Prek editor definition") end
---@param fields prek.KeysFields
---@return prek.KeysAction
function ActionsAPI.keys(fields) error("Prek editor definition") end
---@param fields prek.RemapFields
---@return prek.RemapAction
function ActionsAPI.remap(fields) error("Prek editor definition") end
---@param fields prek.CommandFields
---@return prek.CommandAction
function ActionsAPI.command(fields) error("Prek editor definition") end
---Declaration only; live adapter execution remains pending.
---@param fields prek.MorfFields
---@return prek.MorfActionDeclaration
function ActionsAPI.morf(fields) error("Prek editor definition") end
---Declaration only; live adapter execution remains pending.
---@param fields prek.HyprlandFields
---@return prek.HyprlandAction
function ActionsAPI.hyprland(fields) error("Prek editor definition") end
---Declaration only; live adapter execution remains pending.
---@param fields prek.PowerFields
---@return prek.PowerActionDeclaration
function ActionsAPI.power(fields) error("Prek editor definition") end

return prek
