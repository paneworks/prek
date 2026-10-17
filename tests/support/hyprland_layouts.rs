use super::*;
use prek::hyprland::ContextSampler;
use std::sync::Mutex;

fn document() -> String {
    serde_json::json!({"schema":1,"workspaces":[{
        "id":1,"output":"panel","monitor":0,"active":true,"special":false,
        "layout":"scrolling","windows":[{
            "id":"0x1234:abc","rect":[0,20,500,700],"hidden":false,"floating":false,
            "grouped":false,"fullscreen":0,"column":{"index":0,"row":0}
        }],"columns":[{"index":0,"width":0.5,"members":["0x1234:abc"]}]
    }]})
    .to_string()
}

#[test]
fn native_layout_sampling_uses_only_the_closed_query_and_brackets_provider_and_lock() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let server = Server::with_version(
        move |request| {
            seen.lock()
                .unwrap()
                .push(String::from_utf8(request.to_vec()).unwrap());
            match request {
                b"j/locked" => "{\"locked\":false}".into(),
                b"j/status" => "{\"configProvider\":\"lua\"}".into(),
                _ => {
                    assert_eq!(
                        request,
                        include_bytes!("../../src/hyprland/layouts/query.lua")
                    );
                    document()
                }
            }
        },
        Duration::ZERO,
        "0.56.2",
    );
    let client = server.client();
    let mut sampler = ContextSampler::default();
    let snapshot = sampler.sample_layouts(&client, 9).unwrap();
    assert_eq!((snapshot.generation, snapshot.revision), (9, 1));
    assert_eq!(snapshot.workspaces[0].windows[0].id, "0x1234:abc");
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        [
            "j/locked",
            "j/status",
            include_str!("../../src/hyprland/layouts/query.lua"),
            "j/status",
            "j/locked"
        ]
    );
    assert_eq!(sampler.sample_layouts(&client, 9).unwrap().revision, 2);
    assert!(!sampler.inhibited());
    assert!(sampler.sample_layouts(&client, 0).is_err());
}

#[test]
fn unsupported_native_layout_versions_and_providers_never_run_repl_or_dispatch() {
    for version in ["0.54.3", "0.55.4", "0.56.3", "0.56.2"] {
        let server = Server::with_version(
            move |request| match request {
                b"j/locked" => "{\"locked\":false}".into(),
                b"j/status" => "{\"configProvider\":\"hyprlang\"}".into(),
                _ => panic!("unsupported backend performed extra IPC"),
            },
            Duration::ZERO,
            version,
        );
        if version == "0.56.3" {
            assert!(Client::open(&server.directory, "test", uid(), &test_executable()).is_err());
        } else {
            assert!(
                ContextSampler::default()
                    .sample_layouts(&server.client(), 1)
                    .is_err()
            );
        }
    }
}

#[test]
fn native_layout_trailing_lock_is_latched_even_with_a_malformed_document() {
    let locked = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&locked);
    let repl = Arc::new(AtomicU64::new(0));
    let calls = Arc::clone(&repl);
    let server = Server::with_version(
        move |request| match request {
            b"j/locked" => format!("{{\"locked\":{}}}", observed.load(Ordering::Relaxed)),
            b"j/status" => "{\"configProvider\":\"lua\"}".into(),
            _ => {
                assert!(request.starts_with(b"/repl "));
                calls.fetch_add(1, Ordering::Relaxed);
                observed.store(true, Ordering::Relaxed);
                "malformed".into()
            }
        },
        Duration::ZERO,
        "0.56.2",
    );
    let mut sampler = ContextSampler::default();
    let client = server.client();
    assert!(sampler.sample_layouts(&client, 1).is_err());
    assert!(sampler.inhibited());
    locked.store(false, Ordering::Relaxed);
    assert!(sampler.sample_layouts(&client, 1).is_err());
    assert_eq!(repl.load(Ordering::Relaxed), 1);
}

#[test]
fn changed_native_layout_provider_and_unframed_replies_cannot_publish_context() {
    for changed in [false, true] {
        let statuses = AtomicU64::new(0);
        let server = Server::with_version(
            move |request| match request {
                b"j/locked" => "{\"locked\":false}".into(),
                b"j/status" => {
                    let call = statuses.fetch_add(1, Ordering::Relaxed);
                    serde_json::json!({"configProvider":if changed && call>0 {"hyprlang"}else{"lua"}}).to_string()
                }
                _ => {
                    assert!(request.starts_with(b"/repl "));
                    if changed {
                        document()
                    } else {
                        format!("{} trailing", document())
                    }
                }
            },
            Duration::ZERO,
            "0.56.2",
        );
        assert!(
            ContextSampler::default()
                .sample_layouts(&server.client(), 1)
                .is_err()
        );
    }
}

#[test]
fn all_native_layout_round_trips_share_one_request_budget() {
    let server = Server::with_version(
        |request| match request {
            b"j/locked" => "{\"locked\":false}".into(),
            b"j/status" => "{\"configProvider\":\"lua\"}".into(),
            _ => document(),
        },
        Duration::from_millis(90),
        "0.56.2",
    );
    let client = server.client();
    let start = Instant::now();
    assert!(
        ContextSampler::default()
            .sample_layouts(&client, 1)
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(500));
}
