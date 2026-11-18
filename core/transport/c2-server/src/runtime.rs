use crate::config::ServerIpcConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerRuntimeOptions {
    pub async_worker_threads: usize,
    pub max_blocking_threads: usize,
}

impl ServerRuntimeOptions {
    pub fn from_config(config: &ServerIpcConfig) -> Self {
        let derivation = derive_blocking_pool(config).expect("valid server thread pool");
        Self {
            async_worker_threads: derivation.async_worker_threads,
            max_blocking_threads: derivation.max_blocking_threads,
        }
    }
}

/// Derivation inputs for the server's Tokio pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PoolDerivation {
    /// Host parallelism `P`.
    pub(crate) parallelism: usize,
    /// Execution workers `W` (resource callbacks).
    pub(crate) execution_workers: usize,
    /// Inbound chunk-copy concurrency gate `C = clamp(P, 4, 64)`.
    pub(crate) copy_permits: usize,
    /// Stream item encoding concurrency gate `E = clamp(P, 4, 64)`.
    pub(crate) encoding_permits: usize,
    /// Blocking pool size `T = max(512, W + C + E + 16)`.
    pub(crate) max_blocking_threads: usize,
    pub(crate) async_worker_threads: usize,
}

/// Compute host-derived copy and encoding limits with the Tokio thread cap.
pub(crate) fn derive_blocking_pool(
    config: &ServerIpcConfig,
) -> Result<PoolDerivation, std::io::Error> {
    let parallelism = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4);
    let workers =
        usize::try_from(config.max_execution_workers).map_err(|_| invalid_thread_count())?;
    derive_for_host(parallelism, workers)
}

fn invalid_thread_count() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "server thread pool size overflows usize",
    )
}

fn derive_for_host(parallelism: usize, workers: usize) -> Result<PoolDerivation, std::io::Error> {
    if workers == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "max_execution_workers must be >= 1",
        ));
    }
    let gated = parallelism.clamp(4, 64);
    let async_worker_threads = 2;
    let max_blocking_threads = workers
        .checked_add(gated)
        .and_then(|count| count.checked_add(gated))
        .and_then(|count| count.checked_add(16))
        .ok_or_else(invalid_thread_count)?
        .max(512);
    max_blocking_threads
        .checked_add(async_worker_threads)
        .ok_or_else(invalid_thread_count)?;
    Ok(PoolDerivation {
        parallelism,
        execution_workers: workers,
        copy_permits: gated,
        encoding_permits: gated,
        max_blocking_threads,
        async_worker_threads,
    })
}

pub struct ServerRuntimeBuilder;

impl ServerRuntimeBuilder {
    pub fn options(config: &ServerIpcConfig) -> ServerRuntimeOptions {
        ServerRuntimeOptions::from_config(config)
    }

    pub fn build(config: &ServerIpcConfig) -> Result<tokio::runtime::Runtime, std::io::Error> {
        let derivation = derive_blocking_pool(config)?;
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(derivation.async_worker_threads)
            .max_blocking_threads(derivation.max_blocking_threads)
            // The execution scheduler bounds resource callbacks. Encoding and
            // carrier copies must also progress while those callbacks clean up;
            // Tokio's blocking pool is not a second resource-execution gate.
            .enable_all()
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_pool_formula_follows_workers_and_parallelism() {
        for (workers, parallelism, expected_t) in [
            (1usize, 1usize, 512),
            (4, 4, 512),
            (100, 8, 512),
            (1000, 64, 1144),
            (600, 3, 624),
        ] {
            let derivation = derive_for_host(parallelism, workers).unwrap();
            let gated = parallelism.clamp(4, 64);
            assert_eq!(derivation.parallelism, parallelism);
            assert_eq!(derivation.execution_workers, workers);
            assert_eq!(derivation.copy_permits, gated);
            assert_eq!(derivation.encoding_permits, gated);
            assert_eq!(
                derivation.max_blocking_threads,
                512.max(workers + gated * 2 + 16),
                "T formula for W={workers} P={parallelism}"
            );
            assert_eq!(derivation.max_blocking_threads, expected_t);
            assert_eq!(derivation.async_worker_threads, 2);
        }
    }

    #[test]
    fn blocking_pool_overflow_is_rejected() {
        for workers in [usize::MAX, usize::MAX - 32 - 1] {
            let error = derive_for_host(8, workers).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("overflows usize"));
        }
        ServerRuntimeBuilder::build(&ServerIpcConfig::default()).unwrap();
    }

    #[test]
    fn stream_encoding_can_progress_while_the_only_execution_worker_cleans_up() {
        let runtime = ServerRuntimeBuilder::build(&ServerIpcConfig {
            max_execution_workers: 1,
            ..ServerIpcConfig::default()
        })
        .unwrap();
        runtime.block_on(async {
            let (started, entered) = tokio::sync::oneshot::channel();
            let (release, waiting) = std::sync::mpsc::channel();
            let cleanup = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                waiting.recv().unwrap();
            });
            entered.await.unwrap();
            let encoding = tokio::task::spawn_blocking(|| 17);
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), encoding).await;
            release.send(()).unwrap();
            cleanup.await.unwrap();
            assert_eq!(
                result
                    .expect("item encoding must not wait for cleanup")
                    .unwrap(),
                17
            );
        });
    }
}
