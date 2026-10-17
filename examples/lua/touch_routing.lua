-- Consumes one downward two-finger pan, then disables its rule.
local p = require('prek')
local transaction
local function phase(event)
    assert(p.capabilities.has('runtime_rules') and not p.capabilities.has('input_events'), 'routing capability mismatch')
    p.log.info('touch-phase', {stage=event.event.stage, ordinal=event.event.ordinal})
end
p.events.on('rules_changed', function(event)
    assert(transaction and event.accepted and event.revision == '2')
    assert(transaction:status().state == 'accepted' and not p.rules.get('touch-pan').enabled)
    p.log.info('touch-rule-disabled', {revision=event.revision})
end)
return p.config.new {
    mode='route', policy={allowed_devices={'touch'}, allow_runtime_rules=true},
    timing={join_window='5ms', decision_window='200ms'},
    devices={touch={name='Prek local touch', absolute_axes={53,54,57}}},
    rules={p.rules.define {
        id='touch-pan', devices={'touch'}, contexts={'desktop'}, scope='everywhere', delivery='buffer',
        trigger=p.triggers.pan {fingers=2, direction='down', travel='12mm', fallback_travel='0.1norm'},
        action=p.actions.event {name='touch-pan'},
        on_begin=phase, on_update=phase,
        on_end=function(event)
            phase(event)
            transaction=p.rules.transaction()
            transaction:disable('touch-pan')
            transaction:commit()
        end,
    }},
}
