#![cfg(target_os = "linux")]
//! Long virtual-time Lua workload with regeneration; `oslo make soak-lua` runs it.
use prek::{
    lua::{Definition, EventKind},
    protocol::{Epoch, Fence},
};
use std::time::{Duration, Instant};

const WORKLOAD: &str = r#"
local p=require('prek')
local fired=0
p.scheduler.every('5ms', function()
    fired=fired+1
    local scratch={}
    for i=1,64 do scratch[i]=tostring(i)..'-'..fired end
end)
p.events.on('context_changed', function(event)
    local text=string.rep('x', 256)..tostring(event.revision)
end)
p.events.on('ready', function()
    p.scheduler.spawn(function()
        while true do p.scheduler.sleep('10ms') end
    end)
end)
return {version=1}
"#;

fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

#[test]
#[ignore = "long soak; run with oslo make soak-lua"]
fn lua_workload_keeps_bounded_memory_and_tick_latency_across_regenerations() {
    let seconds: u64 = std::env::var("PREK_SOAK_SECONDS")
        .ok()
        .map_or(3600, |value| value.parse().unwrap());
    let definition = Definition::compile(WORKLOAD, "soak.lua").unwrap();
    let ready = serde_json::json!({
        "fence": Fence { broker: Epoch([1, 2]), configuration: 1, session: 1 },
        "seat": "seat0",
    });
    let step = Duration::from_millis(5);
    let generation = Duration::from_secs(60);
    let end = Duration::from_secs(seconds);
    let mut now = Duration::ZERO;
    let mut histogram = vec![0u64; 100_001];
    let mut ticks = 0u64;
    let mut slowest = Duration::ZERO;
    let mut samples = Vec::new();
    let mut generations = 0u64;
    let started = Instant::now();
    while now < end {
        let mut runtime = definition.instantiate().unwrap();
        runtime.dispatch(EventKind::Ready, &ready).unwrap();
        generations += 1;
        let until = (now + generation).min(end);
        let mut revision = 0u64;
        while now < until {
            now += step;
            let tick = Instant::now();
            runtime.tick(now, &serde_json::Value::Null).unwrap();
            let elapsed = tick.elapsed();
            histogram[(elapsed.as_micros() as usize).min(100_000)] += 1;
            slowest = slowest.max(elapsed);
            ticks += 1;
            revision += 1;
            if revision.is_multiple_of(100) {
                runtime
                    .dispatch(
                        EventKind::ContextChanged,
                        &serde_json::json!({"revision": revision}),
                    )
                    .unwrap();
            }
        }
        samples.push(resident_bytes());
    }
    let percentile = |p: u64| {
        let wanted = ticks.saturating_sub(1) * p / 100;
        let mut seen = 0;
        let micros = histogram
            .iter()
            .position(|count| {
                seen += count;
                seen > wanted
            })
            .unwrap();
        Duration::from_micros(micros as u64)
    };
    let warm = samples[samples.len().min(5) - 1];
    let peak = *samples.iter().max().unwrap();
    let receipt = serde_json::json!({
        "virtual_seconds": seconds,
        "wall_seconds": started.elapsed().as_secs_f64(),
        "generations": generations,
        "ticks": ticks,
        "tick_p50_us": percentile(50).as_micros(),
        "tick_p99_us": percentile(99).as_micros(),
        "tick_max_us": slowest.as_micros(),
        "rss_warm_bytes": warm,
        "rss_peak_bytes": peak,
        "rss_final_bytes": *samples.last().unwrap(),
    });
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("lua-soak");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    eprintln!("{receipt}");
    assert!(peak - warm < 4 * 1024 * 1024, "{receipt}");
    assert!(percentile(99) < Duration::from_millis(5), "{receipt}");
}
