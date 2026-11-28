use assert_cmd::Command;
use predicates::prelude::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

const ENDPOINT_ID: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

fn relay_returning(body: &str) -> String {
    let body = body.to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if request.read_line(&mut line).unwrap() == 0 {
                return;
            }
            if line == "\r\n" {
                break;
            }
        }
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    format!("http://{address}")
}

fn registry_command(subcommand: &str, body: &str) -> Command {
    let relay = relay_returning(body);
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["registry", subcommand, "--relay", &relay])
        .env("C2_RELAY_USE_PROXY", "0");
    cmd
}

#[test]
fn registry_help_lists_subcommands() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["registry", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("list-routes"))
        .stdout(predicate::str::contains("resolve").not())
        .stdout(predicate::str::contains("peers"));
}

#[test]
fn registry_requires_relay_url() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["registry", "list-routes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--relay"));
}

#[test]
fn registry_rejects_removed_name_only_resolve_subcommand() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "registry",
        "resolve",
        "--relay",
        "http://127.0.0.1:8080",
        "grid",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "unrecognized subcommand 'resolve'",
    ));
}

#[test]
fn list_routes_rejects_missing_routes_field() {
    registry_command("list-routes", r#"{}"#)
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid JSON"));
}

#[test]
fn list_routes_rejects_malformed_route() {
    registry_command("list-routes", r#"{"routes":[{"name":42}]}"#)
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid JSON"));
}

#[test]
fn list_routes_rejects_missing_route_metadata() {
    registry_command("list-routes", r#"{"routes":[{"name":"grid"}]}"#)
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid JSON"));
}

#[test]
fn list_routes_rejects_missing_new_route_fields() {
    for body in [
        r#"{"routes":[{"name":"grid","relay_id":"relay-a","address":{"http_url":"http://relay-a:8080"},"locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","relay_status":"Alive","conflict":false}]}"#,
        r#"{"routes":[{"name":"grid","relay_id":"relay-a","address":{"http_url":"http://relay-a:8080"},"locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":1234.5,"conflict":false}]}"#,
        r#"{"routes":[{"name":"grid","relay_id":"relay-a","address":{"http_url":"http://relay-a:8080"},"locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":1234.5,"relay_status":"Alive"}]}"#,
    ] {
        registry_command("list-routes", body)
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid JSON"));
    }
}

#[test]
fn list_routes_rejects_invalid_status_and_timestamp() {
    for body in [
        r#"{"routes":[{"name":"grid","relay_id":"relay-a","address":{"http_url":"http://relay-a:8080"},"locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":1234.5,"relay_status":"Unknown","conflict":false}]}"#,
        r#"{"routes":[{"name":"grid","relay_id":"relay-a","address":{"http_url":"http://relay-a:8080"},"locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":"yesterday","relay_status":"Dead","conflict":false}]}"#,
    ] {
        registry_command("list-routes", body)
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid JSON"));
    }
}

#[test]
fn list_routes_prints_each_conflicting_and_unreachable_registration() {
    registry_command(
        "list-routes",
        r#"{"routes":[{"name":"grid","relay_id":"relay-a","address":{"http_url":"http://relay-a:8080"},"locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":1234.5,"relay_status":"Alive","conflict":true},{"name":"grid","relay_id":"relay-b","address":{"http_url":"http://relay-b:8080"},"locality":"peer","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":1235.5,"relay_status":"Alive","conflict":true},{"name":"old","relay_id":"relay-c","address":{"http_url":"http://relay-c:8080"},"locality":"peer","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":1200.0,"relay_status":"Dead","conflict":false}]}"#,
    )
    .assert()
    .success()
    .stdout("grid (relay-a, http://relay-a:8080) - registered_at=1234.5 - Alive - route conflict\ngrid (relay-b, http://relay-b:8080) - registered_at=1235.5 - Alive - route conflict\nold (relay-c, http://relay-c:8080) - registered_at=1200 - Dead\n");
}

#[test]
fn list_routes_empty_preserves_message() {
    registry_command("list-routes", r#"{"routes":[]}"#)
        .assert()
        .success()
        .stdout("No routes registered.\n");
}

#[test]
fn peers_rejects_missing_peer_fields() {
    registry_command("peers", r#"[{"relay_id":"relay-a"}]"#)
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid JSON"));
}

#[test]
fn peers_rejects_malformed_peer_status() {
    registry_command(
        "peers",
        r#"[{"relay_id":"relay-a","address":{"http_url":"http://relay-a:8080"},"route_count":0,"status":"Unknown"}]"#,
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains("invalid JSON"));
}

#[test]
fn registry_displays_valid_responses() {
    registry_command(
        "list-routes",
        r#"{"routes":[{"name":"grid","relay_id":"relay-a","address":{"http_url":"http://relay-a:8080"},"locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":1234.5,"relay_status":"Alive","conflict":false}]}"#,
    )
    .assert()
    .success()
    .stdout("grid (relay-a, http://relay-a:8080) - registered_at=1234.5 - Alive\n");

    registry_command(
        "peers",
        r#"[{"relay_id":"relay-b","address":{"http_url":"http://relay-b:8080"},"route_count":1,"status":"Alive"}]"#,
    )
    .assert()
    .success()
    .stdout("relay-b (http://relay-b:8080) - Alive\n");
}

#[test]
fn registry_displays_iroh_only_routes_and_prefers_http_for_dual_address_peers() {
    let route = serde_json::json!({
        "name": "grid", "relay_id": "nat-relay", "locality": "peer",
        "address": {"iroh": {
            "endpoint_id": ENDPOINT_ID,
            "bridge_url": "https://bridge.example:8443",
            "direct_addrs": ["[2001:db8::1]:5000", "192.0.2.1:5000"]
        }},
        "crm_ns": "example", "crm_name": "Grid", "crm_ver": "1",
        "abi_hash": "abc", "signature_hash": "def", "registered_at": 1234.5,
        "relay_status": "Alive", "conflict": false
    });
    registry_command(
        "list-routes",
        &serde_json::json!({"routes": [route]}).to_string(),
    )
    .assert()
    .success()
    .stdout(format!(
        "grid (nat-relay, iroh://{ENDPOINT_ID}) - registered_at=1234.5 - Alive\n"
    ));

    let peers = serde_json::json!([
        {"relay_id": "nat-relay", "address": {"iroh": {"endpoint_id": ENDPOINT_ID}},
         "route_count": 1, "status": "Alive"},
        {"relay_id": "dual-relay", "address": {
            "http_url": "http://relay.example:8080",
            "iroh": {"endpoint_id": ENDPOINT_ID}
        }, "route_count": 0, "status": "Suspect"}
    ]);
    registry_command("peers", &peers.to_string())
        .assert()
        .success()
        .stdout(format!(
            "nat-relay (iroh://{ENDPOINT_ID}) - Alive\ndual-relay (http://relay.example:8080) - Suspect\n"
        ));
}

#[test]
fn registry_rejects_retired_url_fields() {
    for (command, body) in [
        (
            "list-routes",
            r#"{"routes":[{"name":"grid","relay_id":"relay-a","relay_url":"http://relay-a:8080","locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def","registered_at":1234.5,"relay_status":"Alive","conflict":false}]}"#,
        ),
        (
            "peers",
            r#"[{"relay_id":"relay-a","url":"http://relay-a:8080","route_count":0,"status":"Alive"}]"#,
        ),
        (
            "peers",
            r#"[{"relay_id":"relay-a","url":"http://relay-a:8080","address":{"http_url":"http://relay-a:8080"},"route_count":0,"status":"Alive"}]"#,
        ),
    ] {
        registry_command(command, body)
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid JSON"));
    }
}

#[test]
fn peers_rejects_invalid_mesh_addresses() {
    for address in [
        serde_json::json!({}),
        serde_json::json!({"http_url": "iroh://wrong-field"}),
        serde_json::json!({"iroh": {}}),
        serde_json::json!({"iroh": {"endpoint_id": ENDPOINT_ID.to_ascii_uppercase()}}),
        serde_json::json!({"iroh": {"endpoint_id": "0123"}}),
        serde_json::json!({"iroh": {"endpoint_id": ENDPOINT_ID, "bridge_url": "not a URL"}}),
        serde_json::json!({"iroh": {"endpoint_id": ENDPOINT_ID, "direct_addrs": ["192.0.2.1"]}}),
    ] {
        let peers = serde_json::json!([
            {"relay_id": "relay-a", "address": address, "route_count": 0, "status": "Alive"}
        ]);
        registry_command("peers", &peers.to_string())
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid JSON"));
    }
}

fn force_leave_relay(status: u16, body: &str) -> (String, std::thread::JoinHandle<()>) {
    use std::io::Read;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = body.to_string();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        request.read_line(&mut line).unwrap();
        assert_eq!(line, "POST /_admin/force-leave HTTP/1.1\r\n");
        let mut length = None;
        loop {
            line.clear();
            assert_ne!(request.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut bytes = vec![0; length.unwrap()];
        request.read_exact(&mut bytes).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::json!({"relay_id":"relay-c"})
        );
        write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    (format!("http://{address}"), handle)
}

#[test]
fn force_leave_requires_target_and_relay() {
    for args in [
        vec!["registry", "force-leave"],
        vec!["registry", "force-leave", "relay-c"],
        vec!["registry", "force-leave", "--relay", "http://127.0.0.1:1"],
    ] {
        Command::cargo_bin("c3")
            .unwrap()
            .args(args)
            .assert()
            .code(2);
    }
    Command::cargo_bin("c3")
        .unwrap()
        .args(["registry", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("force-leave"));
}

#[test]
fn force_leave_posts_target_and_prints_text_success() {
    for flag in ["--relay", "-r"] {
        let (relay, server) = force_leave_relay(200, r#"{"force_left":"relay-c"}"#);
        Command::cargo_bin("c3")
            .unwrap()
            .args(["registry", "force-leave", "relay-c", flag, &relay])
            .env("C2_RELAY_USE_PROXY", "0")
            .assert()
            .code(0)
            .stdout("Force-left relay-c.\n")
            .stderr("");
        server.join().unwrap();
    }
}

#[test]
fn force_leave_reports_relay_rejections_with_reason_and_exit_one() {
    for (status, reason) in [
        (400, "invalid relay_id"),
        (404, "unknown relay relay-c"),
        (409, "cannot force-leave this relay"),
        (409, "relay relay-c is Alive"),
    ] {
        let (relay, server) = force_leave_relay(status, reason);
        Command::cargo_bin("c3")
            .unwrap()
            .args(["registry", "force-leave", "relay-c", "--relay", &relay])
            .env("C2_RELAY_USE_PROXY", "0")
            .assert()
            .code(1)
            .stdout("")
            .stderr(predicate::str::contains(reason))
            .stderr(predicate::str::contains(status.to_string()));
        server.join().unwrap();
    }
}

#[test]
fn force_leave_rejects_malformed_or_mismatched_success_response() {
    for body in [
        "not json",
        "{}",
        r#"{"force_left":3}"#,
        r#"{"force_left":"relay-other"}"#,
    ] {
        let (relay, server) = force_leave_relay(200, body);
        Command::cargo_bin("c3")
            .unwrap()
            .args(["registry", "force-leave", "relay-c", "--relay", &relay])
            .env("C2_RELAY_USE_PROXY", "0")
            .assert()
            .code(1)
            .stdout("");
        server.join().unwrap();
    }
}

#[test]
fn force_leave_reports_network_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    Command::cargo_bin("c3")
        .unwrap()
        .args(["registry", "force-leave", "relay-c", "--relay", &relay])
        .env("C2_RELAY_USE_PROXY", "0")
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains("request failed"));
}
