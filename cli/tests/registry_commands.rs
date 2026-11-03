use assert_cmd::Command;
use predicates::prelude::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

fn relay_returning(body: &'static str) -> String {
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

fn registry_command(subcommand: &str, body: &'static str) -> Command {
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
        r#"[{"relay_id":"relay-a","url":"http://relay-a:8080","route_count":0,"status":"Unknown"}]"#,
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains("invalid JSON"));
}

#[test]
fn registry_displays_valid_responses() {
    registry_command(
        "list-routes",
        r#"{"routes":[{"name":"grid","relay_id":"relay-a","relay_url":"http://relay-a:8080","locality":"local","crm_ns":"example","crm_name":"Grid","crm_ver":"1","abi_hash":"abc","signature_hash":"def"}]}"#,
    )
    .assert()
    .success()
    .stdout("grid\n");

    registry_command(
        "peers",
        r#"[{"relay_id":"relay-b","url":"http://relay-b:8080","route_count":1,"status":"Alive"}]"#,
    )
    .assert()
    .success()
    .stdout("relay-b (http://relay-b:8080) - Alive\n");
}
