use crate::config::ServerIpcConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerRuntimeOptions {
    pub async_worker_threads: usize,
    pub max_blocking_threads: usize,
}

impl ServerRuntimeOptions {
    pub fn from_config(config: &ServerIpcConfig) -> Self {
        Self {
            async_worker_threads: 2,
            max_blocking_threads: config.max_execution_workers as usize,
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
            .max_blocking_threads(options.max_blocking_threads)
            .enable_all()
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_options_derive_blocking_threads_from_server_config() {
        let config = ServerIpcConfig {
            max_execution_workers: 7,
            ..ServerIpcConfig::default()
        };

        let options = ServerRuntimeBuilder::options(&config);

        assert_eq!(options.async_worker_threads, 2);
        assert_eq!(options.max_blocking_threads, 7);
    }
}
