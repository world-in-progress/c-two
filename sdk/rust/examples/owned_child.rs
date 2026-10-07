//! Launch one child with the private receiver as stdin; keep the owner endpoint
//! in this controller. Usage: `owned_child PROGRAM [ARG ...]`.
//! `C2_OWNER_HOLD_SECONDS` controls this example's demonstration duration and
//! `C2_OWNER_RELEASE_MODE=drop` demonstrates ordinary owner destruction.
//! Neither environment value carries a capability.
use c_two::owner_control_pair;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let program = args.next().ok_or("usage: owned_child PROGRAM [ARG ...]")?;
    let (mut keepalive, mut receiver) = owner_control_pair()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(receiver.take_stdio()?)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    println!("CONTROLLER_SPAWNED pid={}", child.id());
    let hold = std::env::var("C2_OWNER_HOLD_SECONDS")
        .unwrap_or_else(|_| "5".into())
        .parse::<f64>()?;
    let deadline = Instant::now() + Duration::from_secs_f64(hold);
    let mut next_marker = Instant::now();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!("owned child exited before owner release: {status}").into());
        }
        if Instant::now() >= next_marker {
            println!("OWNER_BOUND_HOLDING");
            next_marker = Instant::now() + Duration::from_millis(500);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if std::env::var("C2_OWNER_RELEASE_MODE").as_deref() == Ok("drop") {
        drop(keepalive);
    } else {
        keepalive.shutdown();
        drop(keepalive);
    }
    println!("CONTROLLER_RELEASED");
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("owned child failed: {status}").into());
    }
    Ok(())
}
