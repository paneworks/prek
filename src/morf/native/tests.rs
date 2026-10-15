use super::*;
use crate::{config::MorfAction, protocol::Epoch};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};

fn state() -> ShellState {
    ShellState {
        version: 1,
        role: "shell".into(),
        epoch: "a".repeat(32),
        revision: 2,
        phase: ShellPhase::Configured,
        compositor_pid: 123,
        compositor_start: u64::MAX,
        compositor_uid: 1000,
        output: "HEADLESS-1".into(),
    }
}
fn capabilities() -> Capabilities {
    serde_json::from_value(
        json!({"version":1,"epoch":"a".repeat(32),"ui_generation":"1",
        "root_only":true,"queue":8,"receipts":64,"lease_ms":1000,"grant_ms":250,
        "recognizer_ownership":false}),
    )
    .unwrap()
}
fn scope(capabilities: &Capabilities) -> Scope<'_> {
    Scope {
        capabilities,
        fence: Fence {
            broker: Epoch([u64::MAX, 2]),
            configuration: 3,
            session: 4,
        },
    }
}
fn payload() -> controls::Request {
    controls::Request {
        version: 1,
        output: "HEADLESS-1".into(),
        epoch: "b".repeat(32),
        gesture: format!("{:032x}", 1),
        ordinal: 1,
        action: MorfAction::Apps,
        phase: controls::Phase::Discrete,
        dx: 0.,
        dy: 0.,
        vx: 0.,
        vy: 0.,
    }
}
fn response(status: &str, id: Option<u64>, error: Option<&str>) -> Value {
    let mut result = json!({"version":1,"epoch":"a".repeat(32),"ui_generation":"1","status":status,"error":error});
    if let Some(id) = id {
        result["id"] = json!(id.to_string());
    }
    result
}
fn pair() -> (Transport, UnixStream) {
    let (client, server) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    (
        Transport {
            stream: client,
            failed: false,
        },
        server,
    )
}

#[test]
fn capability_negotiation_rejects_foreign_regressed_or_ownership_claims() {
    let good = capabilities();
    good.validate(&state(), Some(1)).unwrap();
    for (key, bad) in [
        ("version", json!(2)),
        ("epoch", json!("b".repeat(32))),
        ("ui_generation", json!("0")),
        ("root_only", json!(false)),
        ("queue", json!(9)),
        ("receipts", json!(65)),
        ("lease_ms", json!(1001)),
        ("grant_ms", json!(251)),
        ("recognizer_ownership", json!(true)),
    ] {
        let mut value = json!({"version":1,"epoch":"a".repeat(32),"ui_generation":"1",
            "root_only":true,"queue":8,"receipts":64,"lease_ms":1000,"grant_ms":250,
            "recognizer_ownership":false});
        value[key] = bad;
        assert!(
            serde_json::from_value::<Capabilities>(value)
                .unwrap()
                .validate(&state(), Some(1))
                .is_err(),
            "{key}"
        );
    }
    assert!(good.validate(&state(), Some(2)).is_err());
    for invalid in [
        json!(1),
        json!("01"),
        json!("+1"),
        json!("-1"),
        json!("18446744073709551616"),
    ] {
        assert!(serde_json::from_value::<Decimal>(invalid).is_err());
    }
    assert_eq!(
        serde_json::to_value(Decimal(u64::MAX)).unwrap(),
        json!(u64::MAX.to_string())
    );
    assert!(nanos(Duration::MAX).is_err());
}

#[test]
fn real_stream_exchanges_all_typed_operations_without_claiming_root_authority() {
    // A protocol peer on a socket pair; this models replies, not Morf root or
    // logind acceptance. Production entry points retain both authorization gates.
    let (mut wire, server) = pair();
    let producer = std::thread::spawn(move || {
        let mut server = BufReader::new(server);
        for (op, fields, reply) in [
            ("lease", 6, response("accepted", None, None)),
            ("handoff-quiesce", 6, response("accepted", None, None)),
            ("handoff-activate", 6, response("accepted", None, None)),
            ("submit", 8, response("queued", Some(7), None)),
            ("receipt", 6, response("dispatched", Some(7), None)),
            ("handoff-release", 7, response("accepted", None, None)),
            ("revoke", 5, response("accepted", None, None)),
        ] {
            let mut line = String::new();
            server.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request.as_object().unwrap().len(), fields);
            assert_eq!(request["op"], op);
            assert_eq!(request["epoch"], "a".repeat(32));
            assert_eq!(request["ui_generation"], "1");
            assert_eq!(request["fence"]["broker"][0], u64::MAX);
            if op == "submit" {
                assert_eq!(request["request"]["output"], "HEADLESS-1");
                assert_eq!(request["id"], "7");
            }
            if op.starts_with("handoff-") {
                assert_eq!(request["ownership_generation"], "1");
            }
            if op == "handoff-release" {
                assert_eq!(request["descriptors_released"], true);
            }
            // Fragment every response to exercise framing, not one recv/JSON.
            let reply = reply.to_string() + "\n";
            for part in reply.as_bytes().chunks(3) {
                server.get_mut().write_all(part).unwrap();
            }
        }
    });
    let caps = capabilities();
    let request = payload();
    let operations = [
        Operation::Lease {
            expires_ns: nanos(monotonic().unwrap() + Duration::from_millis(900)).unwrap(),
        },
        Operation::HandoffQuiesce {
            ownership_generation: Decimal(1),
        },
        Operation::HandoffActivate {
            ownership_generation: Decimal(1),
        },
        Operation::Submit {
            id: Decimal(7),
            deadline_ns: nanos(monotonic().unwrap() + Duration::from_millis(200)).unwrap(),
            request: &request,
        },
        Operation::Receipt { id: Decimal(7) },
        Operation::HandoffRelease {
            ownership_generation: Decimal(1),
            descriptors_released: true,
        },
        Operation::Revoke,
    ];
    for (operation, expected) in operations.into_iter().zip([
        Status::Accepted,
        Status::Accepted,
        Status::Accepted,
        Status::Queued,
        Status::Dispatched,
        Status::Accepted,
        Status::Accepted,
    ]) {
        let mut sent = false;
        let reply = exchange_control(
            &mut wire,
            scope(&caps),
            operation,
            &state(),
            &mut (Instant::now() + RPC_TIME),
            &mut sent,
        )
        .unwrap();
        assert!(sent);
        assert_eq!(reply.status(), expected);
    }
    producer.join().unwrap();
}

#[test]
fn expired_cross_output_and_zero_id_requests_send_nothing() {
    let (mut wire, mut server) = pair();
    server.set_nonblocking(true).unwrap();
    let caps = capabilities();
    let mut request = payload();
    request.output = "OTHER".into();
    for operation in [
        Operation::Lease {
            expires_ns: nanos(monotonic().unwrap()).unwrap(),
        },
        Operation::Submit {
            id: Decimal(1),
            deadline_ns: nanos(monotonic().unwrap() + GRANT).unwrap(),
            request: &request,
        },
        Operation::Receipt { id: Decimal(0) },
    ] {
        let mut sent = false;
        assert!(
            exchange_control(
                &mut wire,
                scope(&caps),
                operation,
                &state(),
                &mut (Instant::now() + RPC_TIME),
                &mut sent
            )
            .is_err()
        );
        assert!(!sent);
        assert!(!wire.failed);
        assert_eq!(
            server.read(&mut [0u8; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
    let mut sent = false;
    assert_eq!(
        exchange_control(
            &mut wire,
            scope(&caps),
            Operation::Revoke,
            &state(),
            &mut Instant::now(),
            &mut sent
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(!sent);
    assert!(wire.failed);
}

#[test]
fn partial_reply_timeout_retires_the_stream_and_never_resends_a_mutation() {
    let (mut wire, server) = pair();
    let producer = std::thread::spawn(move || {
        let mut server = BufReader::new(server);
        let mut request = String::new();
        server.read_line(&mut request).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&request).unwrap()["op"],
            "revoke"
        );
        server.get_mut().write_all(b"{\"version\":1").unwrap();
        let mut remainder = Vec::new();
        server.read_to_end(&mut remainder).unwrap();
        assert!(
            remainder.is_empty(),
            "a mutation was retried after an incomplete reply"
        );
    });
    let caps = capabilities();
    let mut sent = false;
    let error = exchange_control(
        &mut wire,
        scope(&caps),
        Operation::Revoke,
        &state(),
        &mut (Instant::now() + Duration::from_millis(100)),
        &mut sent,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(sent);
    assert!(wire.failed);
    let error = control_error(&mut wire, Operation::Revoke, sent, error);
    assert!(error.uncertain);
    let mut sent = false;
    assert!(
        exchange_control(
            &mut wire,
            scope(&caps),
            Operation::Revoke,
            &state(),
            &mut (Instant::now() + RPC_TIME),
            &mut sent
        )
        .is_err()
    );
    assert!(!sent);
    producer.join().unwrap();
}

#[test]
fn failed_read_only_receipts_retire_without_reporting_an_uncertain_effect() {
    let (mut wire, server) = pair();
    let producer = std::thread::spawn(move || {
        let mut reader = BufReader::new(server);
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&request).unwrap()["op"],
            "receipt"
        );
        // Closing without a reply leaves the query unresolved, but it cannot
        // have executed an effect or authorized repeating the original action.
    });
    let caps = capabilities();
    let mut sent = false;
    let operation = Operation::Receipt { id: Decimal(7) };
    let error = exchange_control(
        &mut wire,
        scope(&caps),
        operation,
        &state(),
        &mut (Instant::now() + RPC_TIME),
        &mut sent,
    )
    .unwrap_err();
    let error = control_error(&mut wire, operation, sent, error);
    assert!(sent);
    assert!(!error.uncertain);
    assert!(wire.failed);
    producer.join().unwrap();
}

#[test]
fn foreign_malformed_and_oversized_replies_retire_the_connection() {
    let caps = capabilities();
    let mut bad_replies = Vec::new();
    for (field, value) in [
        ("epoch", json!("b".repeat(32))),
        ("id", json!("8")),
        ("ui_generation", json!("2")),
        ("status", json!("accepted")),
        ("extra", json!(true)),
        ("error", json!("future_error")),
    ] {
        let mut reply = response("queued", Some(7), None);
        reply[field] = value;
        bad_replies.push(reply.to_string() + "\n");
    }
    bad_replies.push("x".repeat(MAX_REPLY + 1) + "\n");
    let mut absent_error = response("queued", Some(7), None);
    absent_error.as_object_mut().unwrap().remove("error");
    bad_replies.push(absent_error.to_string() + "\n");
    for bytes in bad_replies {
        let (mut wire, server) = pair();
        let producer = std::thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let _ = reader.get_mut().write_all(bytes.as_bytes());
        });
        let request = payload();
        let mut sent = false;
        let operation = Operation::Submit {
            id: Decimal(7),
            deadline_ns: nanos(monotonic().unwrap() + GRANT).unwrap(),
            request: &request,
        };
        assert!(
            exchange_control(
                &mut wire,
                scope(&caps),
                operation,
                &state(),
                &mut (Instant::now() + RPC_TIME),
                &mut sent
            )
            .is_err()
        );
        assert!(sent);
        assert!(wire.failed);
        producer.join().unwrap();
    }
}

#[test]
fn newer_vm_refusal_is_observable_but_cannot_be_accepted_as_success() {
    let caps = capabilities();
    let mut value = response("rejected", None, Some("ui_replaced"));
    value["ui_generation"] = json!("2");
    let reply: Reply = serde_json::from_value(value.clone()).unwrap();
    reply.validate(scope(&caps), &Operation::Revoke).unwrap();
    assert_eq!(reply.refusal(), Some(Refusal::UiReplaced));
    assert_eq!(reply.ui_generation(), 2);
    value["status"] = json!("accepted");
    value["error"] = Value::Null;
    assert!(
        serde_json::from_value::<Reply>(value.clone())
            .unwrap()
            .validate(scope(&caps), &Operation::Revoke)
            .is_err()
    );
    value["status"] = json!("rejected");
    value["error"] = json!("ui_replaced");
    value["ui_generation"] = json!("0");
    assert!(
        serde_json::from_value::<Reply>(value)
            .unwrap()
            .validate(scope(&caps), &Operation::Revoke)
            .is_err()
    );
}

#[test]
fn local_root_gate_uses_actual_process_credentials() {
    // SAFETY: read-only credential query; no modeled UID enters production.
    assert_eq!(require_root().is_ok(), unsafe { libc::geteuid() } == 0);
}
