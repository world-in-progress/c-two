//! Independent fixed IPC14 records; deliberately no c2-wire or metadata codec.
use c2_local::{EndpointReapResult, LocalEndpointContext, LocalListener};
use std::{
    io::{self, Write},
    path::Path,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const HELLO14: &[u8; 25] = b"\x15\0\0\0\0\0\0\0\0\0\0\0\x04\0\0\0C2IP\x01\x0e\0\0\0";

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let context = if cfg!(windows) {
        LocalEndpointContext::default_for_platform()?
    } else {
        LocalEndpointContext::with_unix_root(Path::new(&args[2]))?
    };
    let endpoint = context.endpoint(&args[1])?;
    let reply: &[u8; 25] = match args[3].as_str() {
        "mismatch15" => b"\x15\0\0\0\0\0\0\0\0\0\0\0\x06\0\0\0C2IP\x03\x0f\0\0\0",
        "mismatch13" => b"\x15\0\0\0\0\0\0\0\0\0\0\0\x06\0\0\0C2IP\x03\x0d\0\0\0",
        "mismatch-max" => b"\x15\0\0\0\0\0\0\0\0\0\0\0\x06\0\0\0C2IP\x03\xff\xff\xff\xff",
        "bad-magic" => b"\x15\0\0\0\0\0\0\0\0\0\0\0\x06\0\0\0C2IQ\x03\x0f\0\0\0",
        "accepted15" => b"\x15\0\0\0\0\0\0\0\0\0\0\0\x06\0\0\0C2IP\x02\x0f\0\0\0",
        "mismatch14" => b"\x15\0\0\0\0\0\0\0\0\0\0\0\x06\0\0\0C2IP\x03\x0e\0\0\0",
        "probe" => HELLO14, // Bind/close only: parent checks native lease release after exit.
        _ => return Err(io::Error::other("unknown fixed reply")),
    };
    let mut listener = LocalListener::bind(&endpoint)?;
    let mut connections = 0;
    let mut extra_bytes = 0;
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        if args[3] == "probe" {
            return Ok(());
        }
        println!("READY");
        io::stdout().flush()?;
        let mut stream = listener.accept().await?;
        connections += 1;
        let mut hello = [0; 25];
        stream.read_exact(&mut hello).await?;
        if &hello != HELLO14 {
            return Err(io::Error::other("not the golden Hello14"));
        }
        stream.write_all(reply).await?;
        stream.flush().await?;
        let mut extra = [0; 1];
        extra_bytes = match stream.read(&mut extra).await {
            Ok(n) => n,
            Err(e)
                if cfg!(windows)
                    && matches!(
                        e.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
            {
                0
            }
            Err(e) => return Err(e),
        };
        if extra_bytes != 0 {
            return Err(io::Error::other("client sent metadata"));
        }
        // Parent stdin acknowledges completion of the SDK call and its cleanup.
        // Keep the listener live until then so a reconnect cannot go unnoticed.
        let mut done = tokio::io::stdin();
        tokio::select! {
            biased;
            second = listener.accept() => {
                drop(second?);
                connections += 1;
                Err(io::Error::other("client reconnected after fixed rejection"))
            }
            result = done.read_u8() => result.map(|_| ()),
        }
    })
    .await;
    // Every error/timeout drops the stream future, then closes the native listener.
    let cleanup = listener.close();
    println!("COUNTS {connections} {extra_bytes}");
    if !matches!(
        cleanup,
        EndpointReapResult::Reaped | EndpointReapResult::NotApplicable
    ) {
        return Err(io::Error::other(format!("listener cleanup: {cleanup:?}")));
    }
    result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "fixed peer interaction"))?
}
