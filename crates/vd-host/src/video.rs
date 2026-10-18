//! Video pipeline for the host: capture → encode → broadcast.
//!
//! One capture thread feeds every connected client. Each client gets a
//! single-slot channel, so a client that cannot keep up simply misses frames
//! instead of forcing unbounded buffering or slowing everyone down.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use vd_core::capture::FrameSource;
use vd_core::codec::{encode_jpeg, DEFAULT_JPEG_QUALITY};
use vd_core::video::{FrameHeader, VideoFrame};

/// Default stream size while the synthetic source is in use.
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 800;
/// Target frames per second.
pub const DEFAULT_FPS: u32 = 30;

/// Fans encoded frames out to connected clients.
#[derive(Default)]
pub struct Broadcaster {
    subscribers: Mutex<Vec<SyncSender<Arc<VideoFrame>>>>,
}

impl Broadcaster {
    /// Creates an empty broadcaster.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers a new client. The returned receiver has room for one frame.
    pub fn subscribe(&self) -> Receiver<Arc<VideoFrame>> {
        let (sender, receiver) = sync_channel(1);
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.push(sender);
        debug!("video subscriber added ({} total)", subscribers.len());
        receiver
    }

    /// Whether anybody is watching (used to avoid encoding into the void).
    ///
    /// Dead subscribers are pruned by [`publish`](Self::publish), so this can
    /// briefly report `true` after the last client disconnects.
    pub fn has_subscribers(&self) -> bool {
        !self.subscribers.lock().unwrap().is_empty()
    }

    /// Publishes a frame to every client, dropping it for slow ones.
    pub fn publish(&self, frame: Arc<VideoFrame>) {
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|sender| match sender.try_send(frame.clone()) {
            Ok(()) => true,
            // The client is behind: skip this frame rather than queueing.
            Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

/// Spawns the capture/encode loop feeding `broadcaster`.
pub fn spawn_capture(
    broadcaster: Arc<Broadcaster>,
    mut source: Box<dyn FrameSource + Send>,
    fps: u32,
) {
    let frame_interval = Duration::from_micros(1_000_000 / fps.max(1) as u64);
    info!("capture source: {}", source.describe());

    thread::spawn(move || {
        let mut sequence = 0u64;
        let mut streaming = false;
        let mut published = 0u32;
        let mut last_report = Instant::now();
        loop {
            let started = Instant::now();

            // Only capture while somebody is watching.
            let watching = broadcaster.has_subscribers();
            if watching != streaming {
                streaming = watching;
                if watching {
                    info!("viewer connected — streaming");
                } else {
                    info!("no viewers — capture paused");
                }
            }

            if watching {
                match source.next_frame() {
                    Ok(Some(captured)) => match encode_jpeg(&captured, DEFAULT_JPEG_QUALITY) {
                        Ok(encoded) => {
                            let header = FrameHeader {
                                sequence,
                                width: encoded.width,
                                height: encoded.height,
                                format: encoded.format,
                                timestamp_us: captured.timestamp_us,
                            };
                            sequence = sequence.wrapping_add(1);
                            published += 1;
                            broadcaster.publish(Arc::new(VideoFrame::new(header, encoded.data)));
                        }
                        Err(err) => warn!("could not encode a frame: {err}"),
                    },
                    Ok(None) => {}
                    Err(err) => {
                        error!("capture failed: {err}");
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }

            if let Some(remaining) = frame_interval.checked_sub(started.elapsed()) {
                thread::sleep(remaining);
            }

            // Report the achieved rate so encode cost is visible in the logs.
            if last_report.elapsed() >= Duration::from_secs(5) {
                let seconds = last_report.elapsed().as_secs_f32();
                if streaming {
                    debug!("encoding at {:.1} fps", published as f32 / seconds);
                }
                published = 0;
                last_report = Instant::now();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use vd_core::video::FrameFormat;

    fn frame(sequence: u64) -> Arc<VideoFrame> {
        Arc::new(VideoFrame::new(
            FrameHeader {
                sequence,
                width: 1,
                height: 1,
                format: FrameFormat::Jpeg,
                timestamp_us: 0,
            },
            vec![1, 2, 3],
        ))
    }

    #[test]
    fn publishes_to_every_subscriber() {
        let broadcaster = Broadcaster::new();
        let a = broadcaster.subscribe();
        let b = broadcaster.subscribe();
        assert!(broadcaster.has_subscribers());

        broadcaster.publish(frame(1));
        assert_eq!(a.try_recv().unwrap().header.sequence, 1);
        assert_eq!(b.try_recv().unwrap().header.sequence, 1);
    }

    #[test]
    fn drops_frames_for_a_slow_subscriber() {
        let broadcaster = Broadcaster::new();
        let receiver = broadcaster.subscribe();

        // The slot holds one frame; the second is dropped rather than queued.
        broadcaster.publish(frame(1));
        broadcaster.publish(frame(2));

        assert_eq!(receiver.try_recv().unwrap().header.sequence, 1);
        assert!(receiver.try_recv().is_err(), "no frame should be queued");
    }

    #[test]
    fn forgets_disconnected_subscribers() {
        let broadcaster = Broadcaster::new();
        {
            let _receiver = broadcaster.subscribe();
        } // dropped
        broadcaster.publish(frame(1));
        assert!(!broadcaster.has_subscribers());
    }
}
