use std::sync::Arc;

use russel_core::config::RuntimeKind;

/// Pluggable stop / destroy for deployed services — mirrors the `Ingress`
/// trait pattern.
///
/// Boot is deliberately excluded: the boot path is runtime-specific
/// (microVM uses Cloud Hypervisor, container uses podman) and asymmetric
/// with teardown. Future work may extract a full `RuntimeDeploy` trait
/// once boot is restructured into swappable stages.
#[async_trait::async_trait]
pub trait RuntimeLifecycle: Send + Sync {
    async fn stop(&self, service_id: &str) -> anyhow::Result<()>;
    async fn destroy(&self, service_id: &str) -> anyhow::Result<()>;
}

/// Return a lifecycle provider for the given runtime kind.
pub fn lifecycle_for(runtime: RuntimeKind) -> Arc<dyn RuntimeLifecycle> {
    match runtime {
        RuntimeKind::Microvm => Arc::new(crate::microvm::MicrovmRunner::new()),
        RuntimeKind::Container => Arc::new(crate::container::ContainerRunner::new()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingMock {
        stop_calls: Mutex<Vec<String>>,
        destroy_calls: Mutex<Vec<String>>,
    }

    impl RecordingMock {
        fn new() -> Self {
            Self {
                stop_calls: Mutex::new(Vec::new()),
                destroy_calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl RuntimeLifecycle for RecordingMock {
        async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
            self.stop_calls.lock().unwrap().push(service_id.to_string());
            Ok(())
        }

        async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
            self.destroy_calls
                .lock()
                .unwrap()
                .push(service_id.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn trait_object_stop_calls_recording_mock() {
        let mock = Arc::new(RecordingMock::new());
        let lifecycle: Arc<dyn RuntimeLifecycle> = mock.clone();
        lifecycle.stop("svc-a").await.unwrap();
        lifecycle.destroy("svc-b").await.unwrap();
        // Inspect calls via the concrete handle (held separately).
        assert_eq!(*mock.stop_calls.lock().unwrap(), vec!["svc-a".to_string()]);
        assert_eq!(
            *mock.destroy_calls.lock().unwrap(),
            vec!["svc-b".to_string()]
        );
    }
}
