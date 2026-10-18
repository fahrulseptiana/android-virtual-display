//! Video pipeline for the host: capture → encode → broadcast.
//!
//! One capture thread feeds every connected client. Each client gets a
//! single-slot channel, so a client that cannot keep up simply misses frames
//! instead of forcing unbounded buffering or slowing everyone down.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use vd_core::capture::{CapturedFrame, FrameSource};
use vd_core::codec::{
    encode_jpeg, fit_frame_aligned, rgba_to_nv12, CodecError, DEFAULT_JPEG_QUALITY,
};
use vd_core::video::{FrameFormat, FrameHeader, VideoFrame};

/// Default stream size while the synthetic source is in use.
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 800;
/// Target frames per second.
pub const DEFAULT_FPS: u32 = 30;

/// An encoder the capture loop can drive, independent of the platform API.
///
/// Not `Send`: the encoder is created and used entirely on the capture thread.
pub trait VideoEncoder {
    /// Encodes one NV12 frame into zero or more access units.
    fn encode(&mut self, nv12: &[u8], timestamp_us: u64) -> Result<Vec<Vec<u8>>, String>;

    /// Codec initialisation data (SPS/PPS), sent to clients before video.
    fn sequence_header(&self) -> &[u8];

    /// Human readable description, for logs.
    fn describe(&self) -> String;
}

/// Builds an encoder once the stream size is known.
///
/// The size is only known after the first frame is captured and scaled, which is
/// why this is a factory rather than an encoder.
pub type EncoderFactory =
    Box<dyn FnOnce(u32, u32, u32) -> Result<Box<dyn VideoEncoder>, String> + Send>;

/// The codec configuration a client needs before it can decode anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    pub format: FrameFormat,
    pub width: u32,
    pub height: u32,
    pub extradata: Vec<u8>,
    /// Bumped whenever the configuration changes, so clients can tell.
    pub generation: u64,
}

/// Fans encoded frames out to connected clients.
pub struct Broadcaster {
    subscribers: Mutex<Vec<SyncSender<Arc<VideoFrame>>>>,
    config: Mutex<Option<StreamConfig>>,
    generations: AtomicU64,
}

impl Default for Broadcaster {
    fn default() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            config: Mutex::new(None),
            generations: AtomicU64::new(0),
        }
    }
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

    /// Records the current stream configuration.
    pub fn set_config(&self, config: StreamConfig) {
        let generation = self.generations.fetch_add(1, Ordering::Relaxed) + 1;
        *self.config.lock().unwrap() = Some(StreamConfig {
            generation,
            ..config
        });
    }

    /// The current stream configuration, if the stream has started.
    pub fn config(&self) -> Option<StreamConfig> {
        self.config.lock().unwrap().clone()
    }
}

/// Spawns the capture/encode loop feeding `broadcaster`.
///
/// `encoder` builds an H.264 encoder on the first frame; when it is `None` the
/// pipeline encodes JPEG instead. Frames larger than `max_size` are downscaled
/// first, so the stream is bounded no matter what the desktop resolution is.
pub fn spawn_capture(
    broadcaster: Arc<Broadcaster>,
    mut source: Box<dyn FrameSource + Send>,
    fps: u32,
    max_size: (u32, u32),
    encoder: Option<EncoderFactory>,
) {
    let frame_interval = Duration::from_micros(1_000_000 / fps.max(1) as u64);
    info!("capture source: {}", source.describe());

    thread::spawn(move || {
        let mut state = CaptureState {
            encoder: None,
            factory: encoder,
            max_size,
            last_report: Instant::now(),
            sequence: 0,
            published: 0,
            scale_ms: 0.0,
            encode_ms: 0.0,
        };
        let mut streaming = false;

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
                    Ok(Some(captured)) => {
                        if let Err(err) = state.handle(&broadcaster, captured, fps) {
                            warn!("could not produce a frame: {err}");
                        }
                    }
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

            // Report the achieved rate so the pipeline cost is visible in logs.
            if state.last_report.elapsed() >= Duration::from_secs(5) {
                let seconds = state.last_report.elapsed().as_secs_f32();
                if streaming {
                    let per_frame = state.published.max(1) as f64;
                    debug!(
                        "encoding at {:.1} fps (scale {:.0} ms, encode {:.0} ms per frame)",
                        state.published as f32 / seconds,
                        state.scale_ms / per_frame,
                        state.encode_ms / per_frame,
                    );
                }
                state.published = 0;
                state.scale_ms = 0.0;
                state.encode_ms = 0.0;
                state.last_report = Instant::now();
            }
        }
    });
}

/// Mutable state owned by the capture thread.
struct CaptureState {
    encoder: Option<Box<dyn VideoEncoder>>,
    factory: Option<EncoderFactory>,
    /// Largest frame to put on the wire.
    max_size: (u32, u32),
    last_report: Instant,
    sequence: u64,
    published: u32,
    scale_ms: f64,
    encode_ms: f64,
}

impl CaptureState {
    /// Scales, encodes and publishes one captured frame.
    fn handle(
        &mut self,
        broadcaster: &Broadcaster,
        captured: CapturedFrame,
        fps: u32,
    ) -> Result<(), CodecError> {
        let timestamp_us = captured.timestamp_us;

        let scale_start = Instant::now();
        // H.264 wants even dimensions; JPEG does not care.
        let frame = fit_frame_aligned(captured, self.max_size.0, self.max_size.1, 2)?;
        self.scale_ms += scale_start.elapsed().as_secs_f64() * 1000.0;

        self.ensure_encoder(broadcaster, frame.width, frame.height, fps);

        let encode_start = Instant::now();
        if let Some(encoder) = self.encoder.as_mut() {
            let nv12 = rgba_to_nv12(frame.width, frame.height, &frame.data)?;
            let units = encoder
                .encode(&nv12, timestamp_us)
                .map_err(CodecError::Codec)?;
            for unit in units {
                self.publish(
                    broadcaster,
                    FrameFormat::H264,
                    frame.width,
                    frame.height,
                    timestamp_us,
                    unit,
                );
            }
        } else {
            let encoded = encode_jpeg(&frame, DEFAULT_JPEG_QUALITY)?;
            self.publish(
                broadcaster,
                encoded.format,
                encoded.width,
                encoded.height,
                timestamp_us,
                encoded.data,
            );
        }
        self.encode_ms += encode_start.elapsed().as_secs_f64() * 1000.0;
        Ok(())
    }

    /// Wraps one encoded unit in a framed message and hands it to the clients.
    fn publish(
        &mut self,
        broadcaster: &Broadcaster,
        format: FrameFormat,
        width: u32,
        height: u32,
        timestamp_us: u64,
        data: Vec<u8>,
    ) {
        broadcaster.publish(Arc::new(VideoFrame::new(
            FrameHeader {
                sequence: self.sequence,
                width,
                height,
                format,
                timestamp_us,
            },
            data,
        )));
        self.sequence += 1;
        self.published += 1;
    }

    /// Creates the H.264 encoder the first time a frame size is known.
    fn ensure_encoder(&mut self, broadcaster: &Broadcaster, width: u32, height: u32, fps: u32) {
        let Some(factory) = self.factory.take() else {
            return;
        };

        match factory(width, height, fps) {
            Ok(encoder) => {
                info!("encoding with {}", encoder.describe());
                broadcaster.set_config(StreamConfig {
                    format: FrameFormat::H264,
                    width,
                    height,
                    extradata: encoder.sequence_header().to_vec(),
                    generation: 0,
                });
                self.encoder = Some(encoder);
            }
            Err(err) => {
                warn!("H.264 unavailable ({err}); falling back to JPEG");
                broadcaster.set_config(StreamConfig {
                    format: FrameFormat::Jpeg,
                    width,
                    height,
                    extradata: Vec::new(),
                    generation: 0,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn config_generation_advances() {
        let broadcaster = Broadcaster::new();
        assert!(broadcaster.config().is_none());

        broadcaster.set_config(StreamConfig {
            format: FrameFormat::H264,
            width: 1280,
            height: 720,
            extradata: vec![1, 2],
            generation: 0,
        });
        let first = broadcaster.config().unwrap();
        assert_eq!(first.format, FrameFormat::H264);

        broadcaster.set_config(StreamConfig {
            format: FrameFormat::H264,
            width: 640,
            height: 360,
            extradata: vec![3],
            generation: 0,
        });
        assert!(broadcaster.config().unwrap().generation > first.generation);
    }
}
