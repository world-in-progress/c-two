use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct RelayProcess(Child);

impl Drop for RelayProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn relay_ephemeral_bind_reports_a_reachable_actual_address() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("c3"));
    for (key, _) in std::env::vars() {
        if key.starts_with("C2_") {
            command.env_remove(key);
        }
    }
    command
        .env("C2_ENV_FILE", "")
        .args(["relay", "--bind", "127.0.0.1:0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(root.path().join("c3.stderr.log")).unwrap());
    #[cfg(unix)]
    command.arg("--ipc-root").arg(root.path());
    let mut relay = RelayProcess(command.spawn().unwrap());
    let stdout = relay.0.stdout.take().unwrap();
    let (announced, announcement) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(url) = line.strip_prefix("C-Two relay listening at ") {
                let _ = announced.send(url.to_owned());
            }
        }
    });
    let url = announcement.recv_timeout(Duration::from_secs(10)).unwrap();
    let address: SocketAddr = url.strip_prefix("http://").unwrap().parse().unwrap();
    assert!(address.ip().is_loopback(), "{url}");
    assert_ne!(address.port(), 0, "{url}");
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(10)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    drop(relay);
    reader.join().unwrap();
}
