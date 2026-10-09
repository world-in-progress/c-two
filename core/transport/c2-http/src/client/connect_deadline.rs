//! Connect-only budget enforcement. Business calls keep their existing controls.
use std::collections::BTreeMap;
use std::future::Future;

use c2_config::ConnectDeadline;
use parking_lot::{Mutex, MutexGuard};

use super::HttpError;

fn exceeded(stage: &'static str) -> HttpError {
    HttpError::LocalCallRejected(
        c2_error::C2Error::new(
            c2_error::ErrorCode::CallDeadlineExceeded,
            format!("connection deadline exceeded during {stage}"),
        )
        .with_details(BTreeMap::from([
            ("operation".into(), "connect".into()),
            ("transport_phase".into(), "pre_dispatch".into()),
            ("stage".into(), stage.into()),
            ("fallback_eligible".into(), "false".into()),
            ("route_withdrawal".into(), "false".into()),
        ])),
    )
}

pub(super) fn check(deadline: ConnectDeadline, stage: &'static str) -> Result<(), HttpError> {
    deadline.check(stage).map_err(|_| exceeded(stage))
}

pub(super) fn lock<'a, T>(
    mutex: &'a Mutex<T>,
    deadline: ConnectDeadline,
    stage: &'static str,
) -> Result<MutexGuard<'a, T>, HttpError> {
    check(deadline, stage)?;
    let guard = match deadline.instant() {
        Some(instant) => mutex
            .try_lock_until(instant)
            .ok_or_else(|| exceeded(stage))?,
        None => mutex.lock(),
    };
    check(deadline, stage)?;
    Ok(guard)
}

pub(super) async fn run<T>(
    deadline: ConnectDeadline,
    stage: &'static str,
    future: impl Future<Output = Result<T, HttpError>>,
) -> Result<T, HttpError> {
    check(deadline, stage)?;
    let result = match deadline.instant() {
        Some(instant) => tokio::time::timeout_at(instant.into(), future)
            .await
            .map_err(|_| exceeded(stage))?,
        None => future.await,
    };
    if matches!(&result, Err(HttpError::LocalCallRejected(error))
        if error.code == c2_error::ErrorCode::CallDeadlineExceeded
            && error.details.get("operation").is_some_and(|operation| operation == "connect"))
    {
        return result;
    }
    check(deadline, stage)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn connect_deadline_zero_does_not_poll_or_create_timer() {
        let deadline =
            ConnectDeadline::start(c2_config::ConnectOptions::new().with_timeout(Duration::ZERO))
                .unwrap();
        // Run without a Tokio context. An expired operation must reject before
        // polling its future or creating a timer/networking executor dependency.
        let error = futures::executor::block_on(run(deadline, "relay_resolve", async {
            panic!("zero budget must not poll control work");
            #[allow(unreachable_code)]
            Ok::<(), HttpError>(())
        }))
        .unwrap_err();
        let HttpError::LocalCallRejected(error) = error else {
            panic!("canonical error required")
        };
        assert_eq!(error.code, c2_error::ErrorCode::CallDeadlineExceeded);
        assert_eq!(error.details["operation"], "connect");
        assert_eq!(error.details["transport_phase"], "pre_dispatch");
        assert_eq!(error.details["fallback_eligible"], "false");
        assert_eq!(error.details["route_withdrawal"], "false");
    }

    #[tokio::test]
    async fn connect_deadline_drops_pending_future() {
        struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let deadline = ConnectDeadline::start(
            c2_config::ConnectOptions::new().with_timeout(Duration::from_millis(30)),
        )
        .unwrap();
        let start = Instant::now();
        let result = run(deadline, "relay_probe", async move {
            let _guard = guard;
            std::future::pending::<Result<(), HttpError>>().await
        })
        .await;
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        let HttpError::LocalCallRejected(error) = result.unwrap_err() else {
            panic!("canonical error required")
        };
        assert_eq!(error.code, c2_error::ErrorCode::CallDeadlineExceeded);
        assert_eq!(error.details["stage"], "relay_probe");
        assert_eq!(error.details["fallback_eligible"], "false");
        assert_eq!(error.details["route_withdrawal"], "false");
    }
}
