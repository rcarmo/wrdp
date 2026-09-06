//! Native Wayland data-control adapter.

use crate::{
    desktop::portal::xdg_desktop::{ClipboardBackend, types::ClipboardData},
    rdp::channels::clipboard::{
        error::{ClipboardError, Result},
        provider::{ClipboardProvider, ClipboardProviderEvent},
    },
};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;

type Backend = Arc<Mutex<Box<dyn ClipboardBackend>>>;
pub struct DataControlClipboardProvider {
    backend: Backend,
    events: mpsc::UnboundedSender<ClipboardProviderEvent>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<ClipboardProviderEvent>>>,
    stopped: Arc<AtomicBool>,
    pending: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}
impl DataControlClipboardProvider {
    pub fn new(backend: Backend) -> Self {
        let (events, receiver) = mpsc::unbounded_channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let tx = events.clone();
        let stop = Arc::clone(&stopped);
        backend
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .on_selection_changed(Box::new(move |mime_types| {
                if !stop.load(Ordering::Relaxed) {
                    let _ = tx.send(ClipboardProviderEvent::SelectionChanged {
                        mime_types,
                        force: true,
                    });
                }
            }));
        Self {
            backend,
            events,
            receiver: Mutex::new(Some(receiver)),
            stopped,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    async fn read_backend<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&dyn ClipboardBackend) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let backend = Arc::clone(&self.backend);
        tokio::task::spawn_blocking(move || {
            let guard = backend
                .lock()
                .map_err(|_| ClipboardError::PortalError("data-control lock poisoned".into()))?;
            operation(guard.as_ref())
        })
        .await
        .map_err(|e| ClipboardError::PortalError(format!("data-control task failed: {e}")))?
    }
    async fn write_backend<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut dyn ClipboardBackend) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let backend = Arc::clone(&self.backend);
        tokio::task::spawn_blocking(move || {
            let mut guard = backend
                .lock()
                .map_err(|_| ClipboardError::PortalError("data-control lock poisoned".into()))?;
            operation(guard.as_mut())
        })
        .await
        .map_err(|e| ClipboardError::PortalError(format!("data-control task failed: {e}")))?
    }
}
#[async_trait]
impl ClipboardProvider for DataControlClipboardProvider {
    fn name(&self) -> &'static str {
        "data-control"
    }
    fn supports_file_transfer(&self) -> bool {
        true
    }
    fn requires_upfront_data(&self) -> bool {
        true
    }
    async fn announce_formats(&self, mime_types: Vec<String>) -> Result<()> {
        let data = {
            let cache = self
                .pending
                .lock()
                .map_err(|_| ClipboardError::PortalError("source cache poisoned".into()))?;
            mime_types
                .iter()
                .filter_map(|m| cache.get(m).map(|v| (m.clone(), v.clone())))
                .collect()
        };
        self.write_backend(move |b| {
            b.set_clipboard(ClipboardData { mime_types, data })
                .map_err(|e| ClipboardError::PortalError(e.to_string()))
        })
        .await?;
        self.pending
            .lock()
            .map_err(|_| ClipboardError::PortalError("source cache poisoned".into()))?
            .clear();
        Ok(())
    }
    async fn read_data(&self, mime_type: &str) -> Result<Vec<u8>> {
        let mime = mime_type.to_string();
        self.read_backend(move |b| {
            b.read_selection(&mime)
                .map(std::option::Option::unwrap_or_default)
                .map_err(|e| ClipboardError::PortalError(e.to_string()))
        })
        .await
    }
    async fn provide_data(&self, mime_type: &str, data: Vec<u8>) -> Result<()> {
        self.pending
            .lock()
            .map_err(|_| ClipboardError::PortalError("source cache poisoned".into()))?
            .insert(mime_type.into(), data.clone());
        let mime = mime_type.to_string();
        self.write_backend(move |b| {
            b.update_source_data(&mime, data)
                .map_err(|e| ClipboardError::PortalError(e.to_string()))
        })
        .await
    }
    async fn complete_transfer(
        &self,
        serial: u32,
        _: &str,
        _: Vec<u8>,
        success: bool,
    ) -> Result<()> {
        self.write_backend(move |b| {
            b.write_done(serial, success)
                .map_err(|e| ClipboardError::PortalError(e.to_string()))
        })
        .await
    }
    fn subscribe(&self) -> mpsc::UnboundedReceiver<ClipboardProviderEvent> {
        self.receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| {
                let (_tx, rx) = mpsc::unbounded_channel();
                rx
            })
    }
    async fn health_check(&self) -> Result<()> {
        self.read_backend(|b| {
            let _ = b.protocol_type();
            Ok(())
        })
        .await
    }
    async fn write_text(&self, text: &str) -> Result<()> {
        let bytes = text.as_bytes().to_vec();
        self.provide_data("text/plain", bytes.clone()).await?;
        self.provide_data("text/plain;charset=utf-8", bytes).await?;
        self.announce_formats(vec!["text/plain".into(), "text/plain;charset=utf-8".into()])
            .await
    }
    async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = &self.events;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::portal::xdg_desktop::{ClipboardProtocol, Result as PortalResult};
    #[derive(Default)]
    struct Mock {
        value: ClipboardData,
    }
    impl ClipboardBackend for Mock {
        fn protocol_type(&self) -> ClipboardProtocol {
            ClipboardProtocol::ExtDataControl
        }
        fn get_clipboard(&self) -> PortalResult<ClipboardData> {
            Ok(self.value.clone())
        }
        fn set_clipboard(&mut self, data: ClipboardData) -> PortalResult<()> {
            self.value = data;
            Ok(())
        }
        fn on_selection_changed(&mut self, _: Box<dyn Fn(Vec<String>) + Send + Sync>) {}
        fn read_selection(&self, m: &str) -> PortalResult<Option<Vec<u8>>> {
            Ok(self.value.data.get(m).cloned())
        }
        fn update_source_data(&mut self, m: &str, d: Vec<u8>) -> PortalResult<()> {
            self.value.data.insert(m.into(), d);
            Ok(())
        }
        fn write_done(&mut self, _: u32, _: bool) -> PortalResult<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn publishes_eager_bytes() {
        let backend: Backend = Arc::new(Mutex::new(Box::<Mock>::default()));
        let provider = DataControlClipboardProvider::new(Arc::clone(&backend));
        provider.write_text("hello").await.unwrap();
        assert_eq!(
            backend
                .lock()
                .unwrap()
                .read_selection("text/plain")
                .unwrap(),
            Some(b"hello".to_vec())
        );
    }
}
