-- Inspects normalized touch input from the protected observation source.
local p = require('prek')
p.events.on('input', function(event)
    assert(p.capabilities.has('input_events') and not p.capabilities.has('runtime_rules'), 'observation capability mismatch')
    local info = p.devices.get(event.input.source)
    if event.input.reason == 'removed' then assert(info == nil, 'removed source remained in catalog')
    else assert(info and info.device.name == 'Prek local touch', 'missing permitted touch catalog entry') end
    p.log.info('touch-input', {kind=event.input.kind,
        contacts=#event.input.contacts, generation=event.input.source.generation})
end)
return p.config.new {
    mode='observe', policy={allow_input_events=true, allowed_devices={'touch'}},
    devices={touch={name='Prek local touch', absolute_axes={53,54,57}}},
    rules={},
}
