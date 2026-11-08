use crate::config::ServerIpcConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerRuntimeOptions {
    pub async_worker_threads: usize,
}

impl ServerRuntimeOptions {
    pub fn from_config(_config: &ServerIpcConfig) -> Self {
        Self {
            async_worker_threads: 2,
        }
    }
}

pub struct ServerRuntimeBuilder;

impl ServerRuntimeBuilder {
    pub fn options(config: &ServerIpcConfig) -> ServerRuntimeOptions {
        ServerRuntimeOptions::from_config(config)
    }

    pub fn build(config: &ServerIpcConfig) -> Result<tokio::runtime::Runtime, std::io::Error> {
        let options = Self::options(config);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(options.async_worker_threads)
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
