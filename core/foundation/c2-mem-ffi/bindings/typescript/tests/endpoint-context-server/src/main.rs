//! Real local byte streams for the Node endpoint-context gate, without RPC or SHM.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use c2_config::{ConfigResolver, ConfigSources, LocalEndpointOptions};
use c2_local::{EndpointReapResult, LocalListener};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

struct Failure {
    phase: &'static str,
    message: String,
    raw_os_error: Option<i32>,
}

impl Failure {
    fn io(phase: &'static str, error: io::Error) -> Self {
        Self {
            phase,
            message: error.to_string(),
            raw_os_error: error.raw_os_error(),
        }
    }
}

fn emit(value: Value) {
    println!("{value}");
    io::stdout().flush().expect("flush fixture status");
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        emit(json!({
            "kind": "error",
            "phase": error.phase,
            "message": error.message,
            "rawOsError": error.raw_os_error,
        }));
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Failure> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() != 3 && arguments.len() != 5 {
        return Err(Failure {
            phase: "arguments",
            message: "usage: probe ROOT ADDRESS | serve ROOT ADDRESS MARKER CONNECTIONS".into(),
            raw_os_error: None,
        });
    }
    let context = ConfigResolver::resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: Some(PathBuf::from(&arguments[1])),
        },
        ConfigSources::empty(),
    )
    .map_err(|error| Failure {
        phase: "capture",
        message: error.to_string(),
        raw_os_error: None,
    })?;
    let endpoint = context
        .endpoint(&arguments[2])
        .map_err(|error| Failure::io("derive", error))?;
    let metadata = |kind| {
        json!({
            "kind": kind,
            "endpointName": endpoint.os_name().to_string_lossy(),
            "namespaceId": context.namespace_id(),
        })
    };
    if arguments[0] == "probe" && arguments.len() == 3 {
        emit(metadata("probe"));
        return Ok(());
    }
    if arguments[0] != "serve" || arguments.len() != 5 {
        return Err(Failure {
            phase: "arguments",
            message: "unknown fixture mode or missing serve arguments".into(),
            raw_os_error: None,
        });
    }
    let marker = arguments[3].parse::<u8>().map_err(|error| Failure {
        phase: "arguments",
        message: error.to_string(),
        raw_os_error: None,
    })?;
    let connections = arguments[4].parse::<usize>().map_err(|error| Failure {
        phase: "arguments",
        message: error.to_string(),
        raw_os_error: None,
    })?;
    let mut listener =
        LocalListener::bind(&endpoint).map_err(|error| Failure::io("bind", error))?;
    emit(metadata("ready"));

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..connections {
        let mut stream = tokio::time::timeout(Duration::from_secs(15), listener.accept())
            .await
            .map_err(|error| Failure {
                phase: "accept",
                message: error.to_string(),
                raw_os_error: None,
            })?
            .map_err(|error| Failure::io("accept", error))?;
        tasks.spawn(async move {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let mut size = [0_u8; 4];
                    match stream.read_exact(&mut size).await {
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                        Err(error) => return Err(error),
                    }
                    let size = u32::from_le_bytes(size) as usize;
                    if size > 256 * 1024 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "fixture message exceeds 256 KiB",
                        ));
                    }
                    let mut reply = vec![0; size + 1];
                    reply[0] = marker;
                    stream.read_exact(&mut reply[1..]).await?;
                    stream
                        .write_all(&(reply.len() as u32).to_le_bytes())
                        .await?;
                    stream.write_all(&reply).await?;
                }
            })
            .await
            .map_err(io::Error::other)?
        });
    }
    while let Some(result) = tasks.join_next().await {
        result
            .map_err(|error| Failure {
                phase: "serve",
                message: error.to_string(),
                raw_os_error: None,
            })?
            .map_err(|error| Failure::io("serve", error))?;
    }
    let cleanup = listener.close();
    if !matches!(cleanup, EndpointReapResult::Reaped) {
        return Err(Failure {
            phase: "close",
            message: format!("native listener cleanup returned {cleanup:?}"),
            raw_os_error: None,
        });
    }
    emit(json!({ "kind": "closed", "connections": connections, "cleanup": "Reaped" }));
    Ok(())
}
