use super::*;

#[test]
fn supervised_task_joins_complete_and_cancel_without_new_events() {
    let definition=Definition::compile(r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('20ms');p.log.info('target')end)
p.scheduler.spawn(function()
 local state,wake=target:join()
 p.log.info('joined',{state=state,generation=wake.context.fence.configuration})
end)
local cancelled=p.scheduler.spawn(function()p.scheduler.sleep('60m');error('cancelled task resumed')end)
p.scheduler.spawn(function()
 local state,wake=cancelled:join()
 p.log.info('cancelled',{state=state,generation=wake.context.fence.configuration})
end)
p.scheduler.after('100ms',function()cancelled:cancel()end)
return {version=1}
"#, "supervised-joins.lua").unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = Vec::new();
    while logs.len() < 3 {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        assert!(
            monotonic().unwrap() < end,
            "supervised joins did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        logs.iter()
            .map(|log| log.message.as_str())
            .collect::<Vec<_>>(),
        ["target", "joined", "cancelled"]
    );
    assert_eq!(logs[1].fields["state"], "completed");
    assert_eq!(logs[2].fields["state"], "cancelled");
    assert_eq!(logs[1].fields["generation"], "1");
    assert_eq!(logs[2].fields["generation"], "1");
}

#[test]
fn replacing_a_vm_retires_its_waiting_task_generation() {
    let binary = Path::new(env!("CARGO_BIN_EXE_prek"));
    let old = Definition::compile(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('1s')end)
p.scheduler.spawn(function()p.log.info('old-waiting');target:join();p.log.info('old-resumed')end)
return {version=1}
"#,
        "old-join.lua",
    )
    .unwrap();
    let mut retired = Session::spawn(binary, fence(1), "seat0", old).unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    loop {
        let logs = retired.step(monotonic().unwrap()).unwrap();
        if !logs.is_empty() {
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].message, "old-waiting");
            break;
        }
        assert!(monotonic().unwrap() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(retired);
    let new = Definition::compile(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('10ms')end)
p.scheduler.spawn(function()
 local state,wake=target:join()
 assert(state=='completed')
 p.log.info('new-joined',{generation=wake.context.fence.configuration})
end)
return {version=1}
"#,
        "new-join.lua",
    )
    .unwrap();
    let mut replacement = Session::spawn(binary, fence(2), "seat0", new).unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    loop {
        let logs = replacement.step(monotonic().unwrap()).unwrap();
        if !logs.is_empty() {
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].message, "new-joined");
            assert_eq!(logs[0].fields["generation"], "2");
            break;
        }
        assert!(monotonic().unwrap() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
