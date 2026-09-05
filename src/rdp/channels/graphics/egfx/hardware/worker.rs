//! Thread-confined hardware encoding. No backend object crosses the thread boundary.
use std::{sync::mpsc, thread};

use super::{H264Frame, HardwareEncoder, HardwareEncoderError, HardwareEncoderResult};

enum Command {
    Encode {
        data: Vec<u8>,
        width: u32,
        height: u32,
        timestamp_ms: u64,
        force_keyframe: bool,
        reply: mpsc::SyncSender<HardwareEncoderResult<Option<H264Frame>>>,
    },
}

/// Sendable command endpoint for a backend created and dropped on its own thread.
/// Closing the endpoint drains queued commands and destroys the backend there.
/// Encoding is synchronous, like the backend API; callers must use a blocking context.
pub struct HardwareEncoderWorker {
    commands: mpsc::SyncSender<Command>,
    backend: &'static str,
    force_keyframe: bool,
}

impl HardwareEncoderWorker {
    pub fn start<F>(factory: F) -> HardwareEncoderResult<Self>
    where
        F: FnOnce() -> HardwareEncoderResult<Box<dyn HardwareEncoder>> + Send + 'static,
    {
        let (commands, requests) = mpsc::sync_channel(1);
        let (ready, startup) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("wrdp-hardware-encoder".into())
            .spawn(move || {
                let mut encoder = match factory() {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(encoder.backend_name())).is_err() {
                    return;
                }
                while let Ok(command) = requests.recv() {
                    match command {
                        Command::Encode {
                            data,
                            width,
                            height,
                            timestamp_ms,
                            force_keyframe,
                            reply,
                        } => {
                            if force_keyframe {
                                encoder.force_keyframe();
                            }
                            let result = encoder.encode_bgra(&data, width, height, timestamp_ms);
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|error| HardwareEncoderError::InitFailed(error.to_string()))?;
        let backend = startup.recv().map_err(|_| {
            HardwareEncoderError::InitFailed("encoder worker exited during startup".into())
        })??;
        Ok(Self {
            commands,
            backend,
            force_keyframe: false,
        })
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend
    }

    pub fn force_keyframe(&mut self) {
        // Coalesce requests locally; never block the async caller on the queue.
        self.force_keyframe = true;
    }

    pub fn encode_bgra(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> HardwareEncoderResult<Option<H264Frame>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Encode {
                data: data.to_vec(),
                width,
                height,
                timestamp_ms,
                force_keyframe: std::mem::take(&mut self.force_keyframe),
                reply,
            })
            .map_err(|_| HardwareEncoderError::EncodeFailed("encoder worker stopped".into()))?;
        result.recv().map_err(|_| {
            HardwareEncoderError::EncodeFailed("encoder worker exited during encoding".into())
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::super::HardwareEncoderStats;
    use super::*;
    use std::{rc::Rc, time::Duration};

    struct ThreadBound {
        owner: thread::ThreadId,
        // Deliberately !Send, like the real VA-API backend.
        keyframe: Rc<std::cell::Cell<bool>>,
        dropped: mpsc::SyncSender<thread::ThreadId>,
    }
    impl HardwareEncoder for ThreadBound {
        fn encode_bgra(
            &mut self,
            _: &[u8],
            _: u32,
            _: u32,
            timestamp: u64,
        ) -> HardwareEncoderResult<Option<H264Frame>> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(Some(H264Frame::new(
                vec![1],
                self.keyframe.get(),
                timestamp,
            )))
        }
        fn force_keyframe(&mut self) {
            assert_eq!(thread::current().id(), self.owner);
            self.keyframe.set(true);
        }
        fn stats(&self) -> HardwareEncoderStats {
            HardwareEncoderStats::new("test", 1000)
        }
        fn backend_name(&self) -> &'static str {
            "test"
        }
    }
    impl Drop for ThreadBound {
        fn drop(&mut self) {
            let _ = self.dropped.send(thread::current().id());
        }
    }

    #[test]
    fn backend_lifetime_stays_on_its_creation_thread() {
        let (owner_tx, owner_rx) = mpsc::sync_channel(1);
        let (drop_tx, drop_rx) = mpsc::sync_channel(1);
        let mut worker = HardwareEncoderWorker::start(move || {
            let owner = thread::current().id();
            owner_tx.send(owner).unwrap();
            Ok(Box::new(ThreadBound {
                owner,
                keyframe: Rc::new(std::cell::Cell::new(false)),
                dropped: drop_tx,
            }))
        })
        .unwrap();
        let owner = owner_rx.recv().unwrap();
        assert_ne!(owner, thread::current().id());
        thread::spawn(move || {
            worker.force_keyframe();
            assert!(
                worker
                    .encode_bgra(&[0; 4], 1, 1, 7)
                    .unwrap()
                    .unwrap()
                    .is_keyframe
            );
        })
        .join()
        .unwrap();
        assert_eq!(drop_rx.recv_timeout(Duration::from_secs(5)).unwrap(), owner);
    }

    #[test]
    fn worker_startup_panic_is_reported_without_hanging() {
        let result = HardwareEncoderWorker::start(|| panic!("simulated driver startup panic"));
        assert!(matches!(result, Err(HardwareEncoderError::InitFailed(_))));
    }

    #[test]
    fn keyframe_requests_are_coalesced_without_queueing() {
        let (commands, _requests) = mpsc::sync_channel(1);
        let mut worker = HardwareEncoderWorker {
            commands,
            backend: "test",
            force_keyframe: false,
        };
        // No worker consumes commands. These calls would deadlock if they
        // attempted to send a message into the bounded queue.
        for _ in 0..100 {
            worker.force_keyframe();
        }
        assert!(worker.force_keyframe);
    }

    #[test]
    fn startup_failure_is_returned() {
        assert!(
            HardwareEncoderWorker::start(|| Err(HardwareEncoderError::InitFailed("test".into())))
                .is_err()
        );
    }
}
