//! Camera capture thread.
//!
//! One dedicated OS thread owns the device and answers commands over a
//! channel. Between commands it only pulls frames off the driver - cheap, so
//! the driver's buffer queue never backs up and a shutter is answered within a
//! frame. A second thread decodes the newest of those frames into a shared
//! preview slot (latest wins, no backlog); on a slow CPU that lowers the
//! preview frame rate, never its freshness.
//!
//! # Why arming exists
//!
//! Stills are taken at `capture_resolution`. Reconfiguring the stream for that
//! takes anywhere from tens of milliseconds to a couple of seconds, and while
//! it happens no preview frames arrive - so the screen freezes on a stale
//! frame, the user reads that as "photo taken", relaxes, and the real shutter
//! fires afterwards on a different pose. That is the single worst thing a
//! photobooth can do.
//!
//! So the reconfiguration is moved off the shutter path: [`CameraCommand::Arm`]
//! switches formats when the countdown *starts* and keeps the preview running
//! from the capture-resolution stream. At zero, [`CameraCommand::Capture`] only
//! has to pull the next frame, so the frame the user sees freeze is the frame
//! that gets stored. [`CameraCommand::Disarm`] restores the preview format
//! after the session.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
#[cfg(not(feature = "fast-jpeg"))]
use image::ImageFormat;
use image::RgbImage;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::{Buffer, query};

use crate::config::{self, CameraBackend, PixelFormatPref};
use crate::layout;

/// Longest edge of a preview frame handed to the UI. Anything larger is
/// downscaled on the preview decoder thread so texture uploads stay cheap.
pub const MAX_PREVIEW_EDGE: u32 = 1280;

/// Frame interval targeted by the mock backend.
const MOCK_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Stream reconfiguration cost the mock backend pretends to pay, in the range
/// real USB cameras take. Arming absorbs it during the countdown; capturing
/// unarmed pays it at the shutter.
const MOCK_ARM_DELAY: Duration = Duration::from_millis(1200);

/// Frame rate requested from the device. Cameras negotiate the closest match.
const REQUESTED_FPS: u32 = 30;

/// Consecutive `frame()` errors tolerated before the thread gives up.
const MAX_CONSECUTIVE_FRAME_ERRORS: u32 = 30;

#[derive(Debug)]
pub enum CameraCommand {
    /// Switch to the capture format ahead of the shutter and keep streaming.
    /// Idempotent: arming an armed camera does nothing.
    Arm,
    /// Keep the next frame. Fast when armed; falls back to a slow in-line
    /// format switch when not.
    Capture,
    /// Return to the preview format.
    Disarm,
    Shutdown,
}

#[derive(Debug)]
pub enum CameraEvent {
    /// The device is streaming; carries a human-readable description.
    Ready(String),
    /// The camera reached the capture format, and how long that took. A long
    /// time here is fine: it happened during the countdown.
    Armed { description: String, took: Duration },
    /// A still finished. Full capture resolution, rotation and mirroring
    /// already applied.
    Captured(Box<Capture>),
    /// Capture failed but the thread is still alive.
    CaptureFailed(String),
    /// Something degraded but recoverable, worth showing in the status log.
    Warning(String),
    /// The thread died; no further events will arrive.
    Fatal(String),
}

/// A kept still, with the moment it was taken.
#[derive(Debug)]
pub struct Capture {
    pub image: RgbImage,
    /// The driver's capture timestamp when the backend gives one, so a frame
    /// that sat in a queue is not mistaken for a fresh one; otherwise the
    /// moment the frame came back from the driver.
    pub taken_at: Instant,
}

impl Capture {
    /// Milliseconds from `shutter_at` to when the frame was taken; negative
    /// when it was taken before the shutter.
    pub fn offset_ms(&self, shutter_at: Instant) -> f32 {
        if self.taken_at >= shutter_at {
            (self.taken_at - shutter_at).as_secs_f32() * 1000.0
        } else {
            -(shutter_at - self.taken_at).as_secs_f32() * 1000.0
        }
    }
}

/// Newest preview frame plus a sequence number so the UI can skip re-uploading
/// a texture it already has.
pub struct PreviewFrame {
    pub image: RgbImage,
    pub seq: u64,
}

pub struct CameraHandle {
    commands: Sender<CameraCommand>,
    events: Receiver<CameraEvent>,
    latest_preview: Arc<Mutex<Option<PreviewFrame>>>,
    join: Option<JoinHandle<()>>,
}

impl CameraHandle {
    /// Start the capture thread. Device errors surface as [`CameraEvent::Fatal`]
    /// rather than failing here, so the UI can show them.
    pub fn spawn(config: config::Camera) -> Self {
        let (command_tx, command_rx) = channel();
        let (event_tx, event_rx) = channel();
        let latest_preview = Arc::new(Mutex::new(None));
        let preview_slot = Arc::clone(&latest_preview);

        let join = std::thread::Builder::new()
            .name("rsbooth-camera".to_string())
            .spawn(move || {
                let result =
                    with_preview_decoder(&config, &preview_slot, |pending| match config.backend {
                        CameraBackend::Webcam => {
                            run_webcam(&config, &command_rx, &event_tx, pending)
                        }
                        CameraBackend::Mock => run_mock(&config, &command_rx, &event_tx, pending),
                    });
                if let Err(error) = result {
                    tracing::error!("camera thread stopped: {error:#}");
                    let _ = event_tx.send(CameraEvent::Fatal(format!("{error:#}")));
                }
            })
            .expect("spawning the camera thread");

        Self {
            commands: command_tx,
            events: event_rx,
            latest_preview,
            join: Some(join),
        }
    }

    /// Prepare for a shutter that is a second or two away. Call at the start
    /// of a countdown so the format switch does not land on the shutter.
    pub fn arm(&self) {
        let _ = self.commands.send(CameraCommand::Arm);
    }

    /// Release the capture format once a session is over.
    pub fn disarm(&self) {
        let _ = self.commands.send(CameraCommand::Disarm);
    }

    pub fn request_capture(&self) {
        let _ = self.commands.send(CameraCommand::Capture);
    }

    /// Drain pending events. Never blocks.
    pub fn poll_events(&self) -> Vec<CameraEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    /// Take the newest preview frame if it is newer than `last_seq`.
    pub fn take_preview(&self, last_seq: u64) -> Option<PreviewFrame> {
        let mut slot = self.latest_preview.lock().expect("preview slot poisoned");
        match slot.as_ref() {
            Some(frame) if frame.seq > last_seq => slot.take(),
            _ => None,
        }
    }
}

impl Drop for CameraHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(CameraCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Newest undecoded frame, handed from the camera thread to the preview
/// decoder. A frame the decoder has not picked up yet is replaced, never
/// queued.
#[derive(Default)]
struct PendingFrame {
    state: Mutex<PendingState>,
    ready: Condvar,
}

#[derive(Default)]
struct PendingState {
    buffer: Option<Buffer>,
    closed: bool,
}

impl PendingFrame {
    fn offer(&self, buffer: Buffer) {
        self.state.lock().expect("pending frame poisoned").buffer = Some(buffer);
        self.ready.notify_one();
    }

    fn close(&self) {
        self.state.lock().expect("pending frame poisoned").closed = true;
        self.ready.notify_one();
    }

    /// Block until a frame is offered. `None` once closed.
    fn take(&self) -> Option<Buffer> {
        let state = self.state.lock().expect("pending frame poisoned");
        let mut state = self
            .ready
            .wait_while(state, |state| state.buffer.is_none() && !state.closed)
            .expect("pending frame poisoned");
        if state.closed {
            None
        } else {
            state.buffer.take()
        }
    }
}

/// Run `stream` - a device loop that offers its frames to the [`PendingFrame`]
/// it is given - alongside the preview decoder thread, and stop the decoder
/// when the loop ends.
fn with_preview_decoder(
    config: &config::Camera,
    preview_slot: &Mutex<Option<PreviewFrame>>,
    stream: impl FnOnce(&PendingFrame) -> Result<()>,
) -> Result<()> {
    let pending = PendingFrame::default();
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("rsbooth-preview".to_string())
            .spawn_scoped(scope, || decode_previews(&pending, config, preview_slot))
            .expect("spawning the preview decoder thread");
        let result = stream(&pending);
        pending.close();
        result
    })
}

/// Decode, shrink and orient the newest offered frame until `pending` is
/// closed.
fn decode_previews(
    pending: &PendingFrame,
    config: &config::Camera,
    preview_slot: &Mutex<Option<PreviewFrame>>,
) {
    let mut sequence = 0u64;
    while let Some(buffer) = pending.take() {
        match decode_buffer(&buffer) {
            Ok(image) => {
                sequence += 1;
                // Shrinking first leaves less to orient.
                let image = orient(layout::thumbnail(image, MAX_PREVIEW_EDGE), config);
                *preview_slot.lock().expect("preview slot poisoned") = Some(PreviewFrame {
                    image,
                    seq: sequence,
                });
            }
            Err(error) => tracing::warn!("decoding a preview frame: {error:#}"),
        }
    }
}

/// Cameras visible to the platform backend, as `(index, name)`.
pub fn list_cameras() -> Result<Vec<(String, String)>> {
    let cameras =
        query(nokhwa::utils::ApiBackend::Auto).map_err(|e| anyhow!("querying cameras: {e}"))?;
    Ok(cameras
        .into_iter()
        .map(|info| {
            (
                info.index().to_string(),
                format!("{} ({})", info.human_name(), info.description()),
            )
        })
        .collect())
}

fn run_webcam(
    config: &config::Camera,
    commands: &Receiver<CameraCommand>,
    events: &Sender<CameraEvent>,
    pending: &PendingFrame,
) -> Result<()> {
    // Required on macOS to prompt for camera permission; a no-op elsewhere.
    #[cfg(target_os = "macos")]
    nokhwa::nokhwa_initialize(|_granted| {});

    let index = CameraIndex::Index(config.index);
    let preview_request = format_request(config.preview_resolution, config.format);
    let mut camera = nokhwa::Camera::new(index, preview_request)
        .map_err(|e| anyhow!("opening camera {}: {e}", config.index))?;
    camera
        .open_stream()
        .map_err(|e| anyhow!("starting the preview stream: {e}"))?;

    let format = camera.camera_format();
    let _ = events.send(CameraEvent::Ready(format!(
        "{} @ {}x{} {} {}fps",
        camera.info().human_name(),
        format.resolution().width(),
        format.resolution().height(),
        format.format(),
        format.frame_rate()
    )));

    let mut consecutive_errors = 0u32;
    let mut armed = false;
    loop {
        match commands.try_recv() {
            Ok(CameraCommand::Shutdown) | Err(TryRecvError::Disconnected) => break,
            Ok(CameraCommand::Arm) => {
                if !armed {
                    match arm_camera(&mut camera, config) {
                        Ok(took) => {
                            armed = true;
                            let format = camera.camera_format();
                            let _ = events.send(CameraEvent::Armed {
                                description: format!(
                                    "{}x{} {}",
                                    format.resolution().width(),
                                    format.resolution().height(),
                                    format.format()
                                ),
                                took,
                            });
                        }
                        Err(error) => {
                            // Shoot from the preview stream rather than lose
                            // the session; the shutter will be slower. The
                            // switch may have stopped part-way, so put the
                            // preview stream back first.
                            tracing::warn!("could not arm the camera: {error:#}");
                            let _ = events.send(CameraEvent::Warning(format!(
                                "could not switch to the capture format: {error:#}"
                            )));
                            restore_preview(&mut camera, config)
                                .context("restoring the preview after a failed arm")?;
                        }
                    }
                }
                continue;
            }
            Ok(CameraCommand::Disarm) => {
                if armed {
                    restore_preview(&mut camera, config).context("restoring the preview format")?;
                    armed = false;
                }
                continue;
            }
            Ok(CameraCommand::Capture) => {
                let result = if armed {
                    grab(&mut camera, config)
                } else {
                    capture_unarmed(&mut camera, config)
                };
                match result {
                    Ok(capture) => {
                        let _ = events.send(CameraEvent::Captured(Box::new(capture)));
                    }
                    Err(error) => {
                        tracing::warn!("capture failed: {error:#}");
                        let _ = events.send(CameraEvent::CaptureFailed(format!("{error:#}")));
                    }
                }
                if !armed {
                    // Restore even when the capture failed. Failing here ends
                    // the thread, since there would be no preview left.
                    restore_preview(&mut camera, config)
                        .context("restoring the preview after an unarmed capture")?;
                }
                continue;
            }
            Err(TryRecvError::Empty) => {}
        }

        match camera.frame() {
            Ok(buffer) => {
                consecutive_errors = 0;
                pending.offer(buffer);
            }
            Err(error) => {
                consecutive_errors += 1;
                tracing::warn!("reading a frame: {error}");
                if consecutive_errors >= MAX_CONSECUTIVE_FRAME_ERRORS {
                    bail!("camera stopped delivering frames: {error}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Ok(())
}

/// Switch to the capture format and burn the warm-up frames, so exposure and
/// white balance have settled before the shutter. Returns how long it took.
fn arm_camera(camera: &mut nokhwa::Camera, config: &config::Camera) -> Result<Duration> {
    let started = Instant::now();
    if config.capture_resolution != config.preview_resolution {
        camera
            .stop_stream()
            .map_err(|e| anyhow!("stopping the preview stream: {e}"))?;
        camera
            .set_camera_requset(format_request(config.capture_resolution, config.format))
            .map_err(|e| anyhow!("switching to the capture format: {e}"))?;
        camera
            .open_stream()
            .map_err(|e| anyhow!("starting the capture stream: {e}"))?;
    }
    for _ in 0..config.warmup_frames {
        camera
            .frame()
            .map_err(|e| anyhow!("reading a warm-up frame: {e}"))?;
    }
    Ok(started.elapsed())
}

fn restore_preview(camera: &mut nokhwa::Camera, config: &config::Camera) -> Result<()> {
    if config.capture_resolution == config.preview_resolution {
        return Ok(());
    }
    camera
        .stop_stream()
        .map_err(|e| anyhow!("stopping the capture stream: {e}"))?;
    camera
        .set_camera_requset(format_request(config.preview_resolution, config.format))
        .map_err(|e| anyhow!("switching to the preview format: {e}"))?;
    camera
        .open_stream()
        .map_err(|e| anyhow!("restarting the preview stream: {e}"))?;
    Ok(())
}

/// Capture from a camera that is not armed - arming failed, or a caller
/// skipped it. The format switch happens here, so the shutter lags by however
/// long that takes. The caller restores the preview format afterwards.
fn capture_unarmed(camera: &mut nokhwa::Camera, config: &config::Camera) -> Result<Capture> {
    tracing::warn!("capturing from an unarmed camera; the shutter will lag");
    let took = arm_camera(camera, config)?;
    tracing::warn!("in-line format switch cost {took:?}");
    grab(camera, config)
}

/// Grab the frame that will be stored: drop the frames the driver already had
/// queued, then keep the next one, so the stored image was exposed after the
/// shutter and not before it. On an armed camera this takes a few tens of
/// milliseconds, so the still matches what was on screen at zero.
fn grab(camera: &mut nokhwa::Camera, config: &config::Camera) -> Result<Capture> {
    for _ in 0..config.capture_discard_frames {
        camera
            .frame()
            .map_err(|e| anyhow!("discarding a stale frame: {e}"))?;
    }
    let buffer = camera
        .frame()
        .map_err(|e| anyhow!("reading the capture frame: {e}"))?;
    let taken_at = stamped_at(&buffer).unwrap_or_else(Instant::now);
    let image = decode_buffer(&buffer).context("decoding the capture frame")?;
    Ok(Capture {
        image: orient(image, config),
        taken_at,
    })
}

/// When the driver stamped `buffer`, mapped onto the [`Instant`] clock. `None`
/// when the backend gives no timestamp.
fn stamped_at(buffer: &Buffer) -> Option<Instant> {
    let stamped = UNIX_EPOCH + buffer.capture_timestamp()?;
    let age = SystemTime::now()
        .duration_since(stamped)
        .unwrap_or_default();
    Instant::now().checked_sub(age)
}

fn format_request(resolution: [u32; 2], preference: PixelFormatPref) -> RequestedFormat<'static> {
    let resolution = Resolution::new(resolution[0], resolution[1]);
    match preference {
        PixelFormatPref::Any => {
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::HighestResolution(resolution))
        }
        PixelFormatPref::Mjpeg => RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new(resolution, FrameFormat::MJPEG, REQUESTED_FPS),
        )),
        PixelFormatPref::Yuyv => RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new(resolution, FrameFormat::YUYV, REQUESTED_FPS),
        )),
    }
}

/// Decode a camera buffer to RGB8.
///
/// nokhwa decodes MJPEG only, and only with its `decoding` feature, so every
/// other format is handled here regardless. See [`decode_mjpeg`] for the JPEG
/// path.
pub fn decode_buffer(buffer: &Buffer) -> Result<RgbImage> {
    let width = buffer.resolution().width();
    let height = buffer.resolution().height();
    let bytes = buffer.buffer();
    match buffer.source_frame_format() {
        FrameFormat::MJPEG => decode_mjpeg(buffer),
        FrameFormat::YUYV => yuyv_to_rgb(bytes, width, height),
        FrameFormat::NV12 => nv12_to_rgb(bytes, width, height),
        FrameFormat::GRAY => gray_to_rgb(bytes, width, height),
        FrameFormat::RAWRGB => raw_to_rgb(bytes, width, height, false),
        FrameFormat::RAWBGR => raw_to_rgb(bytes, width, height, true),
    }
}

/// MJPEG decoding: mozjpeg's SIMD decoder with the `fast-jpeg` feature on, the
/// pure-Rust `image` decoder otherwise.
///
/// Decoding a 720p frame on an aarch64 desktop core: 7.5ms pure-Rust, 2.5ms
/// mozjpeg. A Pi core is several times slower again, so build with
/// `--features fast-jpeg` (implied by `--features pi`) on that hardware.
#[cfg(feature = "fast-jpeg")]
fn decode_mjpeg(buffer: &Buffer) -> Result<RgbImage> {
    buffer
        .decode_image::<RgbFormat>()
        .map_err(|error| anyhow!("decoding an MJPEG frame: {error}"))
}

#[cfg(not(feature = "fast-jpeg"))]
fn decode_mjpeg(buffer: &Buffer) -> Result<RgbImage> {
    Ok(
        image::load_from_memory_with_format(buffer.buffer(), ImageFormat::Jpeg)
            .context("decoding an MJPEG frame")?
            .to_rgb8(),
    )
}

fn expect_len(bytes: &[u8], needed: usize, format: &str) -> Result<()> {
    if bytes.len() < needed {
        bail!(
            "{format} frame is {} bytes, expected at least {needed}",
            bytes.len()
        );
    }
    Ok(())
}

/// BT.601 limited-range YUV -> RGB, the convention UVC cameras use.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = (f32::from(y) - 16.0) * 1.164_383;
    let u = f32::from(u) - 128.0;
    let v = f32::from(v) - 128.0;
    [
        (y + 1.596_027 * v).clamp(0.0, 255.0) as u8,
        (y - 0.391_762 * u - 0.812_968 * v).clamp(0.0, 255.0) as u8,
        (y + 2.017_232 * u).clamp(0.0, 255.0) as u8,
    ]
}

fn yuyv_to_rgb(bytes: &[u8], width: u32, height: u32) -> Result<RgbImage> {
    let pixels = width as usize * height as usize;
    expect_len(bytes, pixels * 2, "YUYV")?;
    let mut out = Vec::with_capacity(pixels * 3);
    // Each 4-byte group carries two pixels sharing one chroma pair.
    for group in bytes[..pixels * 2].chunks_exact(4) {
        let (y0, u, y1, v) = (group[0], group[1], group[2], group[3]);
        out.extend_from_slice(&yuv_to_rgb(y0, u, v));
        out.extend_from_slice(&yuv_to_rgb(y1, u, v));
    }
    RgbImage::from_raw(width, height, out).ok_or_else(|| anyhow!("YUYV frame had the wrong size"))
}

fn nv12_to_rgb(bytes: &[u8], width: u32, height: u32) -> Result<RgbImage> {
    let (w, h) = (width as usize, height as usize);
    let luma_len = w * h;
    expect_len(bytes, luma_len + luma_len / 2, "NV12")?;
    let (luma, chroma) = bytes.split_at(luma_len);
    let mut out = Vec::with_capacity(luma_len * 3);
    for row in 0..h {
        // One chroma row is shared by two luma rows, one chroma pair by two columns.
        let chroma_row = (row / 2) * w;
        for column in 0..w {
            let chroma_index = chroma_row + (column / 2) * 2;
            out.extend_from_slice(&yuv_to_rgb(
                luma[row * w + column],
                chroma[chroma_index],
                chroma[chroma_index + 1],
            ));
        }
    }
    RgbImage::from_raw(width, height, out).ok_or_else(|| anyhow!("NV12 frame had the wrong size"))
}

fn gray_to_rgb(bytes: &[u8], width: u32, height: u32) -> Result<RgbImage> {
    let pixels = width as usize * height as usize;
    expect_len(bytes, pixels, "GRAY")?;
    let mut out = Vec::with_capacity(pixels * 3);
    for &value in &bytes[..pixels] {
        out.extend_from_slice(&[value, value, value]);
    }
    RgbImage::from_raw(width, height, out).ok_or_else(|| anyhow!("GRAY frame had the wrong size"))
}

fn raw_to_rgb(bytes: &[u8], width: u32, height: u32, swap_red_blue: bool) -> Result<RgbImage> {
    let needed = width as usize * height as usize * 3;
    expect_len(bytes, needed, "RAW")?;
    let mut out = bytes[..needed].to_vec();
    if swap_red_blue {
        for pixel in out.chunks_exact_mut(3) {
            pixel.swap(0, 2);
        }
    }
    RgbImage::from_raw(width, height, out).ok_or_else(|| anyhow!("raw frame had the wrong size"))
}

/// Apply the configured rotation, then mirroring. Preview frames and stills
/// both pass through here, so what is saved matches what was on screen.
fn orient(image: RgbImage, config: &config::Camera) -> RgbImage {
    let mut image = match config.rotation {
        90 => image::imageops::rotate90(&image),
        180 => image::imageops::rotate180(&image),
        270 => image::imageops::rotate270(&image),
        _ => image,
    };
    if config.mirror {
        image::imageops::flip_horizontal_in_place(&mut image);
    }
    image
}

/// Synthetic backend: a moving gradient. Lets the booth run end to end with no
/// camera attached (development, WSL, CI).
///
/// It deliberately models the expensive part of a real camera - the stream
/// reconfiguration, [`MOCK_ARM_DELAY`] - so the arm-ahead path is exercised
/// rather than assumed. Capturing without arming pays that cost at the shutter,
/// exactly as a real device would.
fn run_mock(
    config: &config::Camera,
    commands: &Receiver<CameraCommand>,
    events: &Sender<CameraEvent>,
    pending: &PendingFrame,
) -> Result<()> {
    let started = Instant::now();
    let _ = events.send(CameraEvent::Ready("mock camera".to_string()));
    let mut armed = false;
    loop {
        match commands.try_recv() {
            Ok(CameraCommand::Shutdown) | Err(TryRecvError::Disconnected) => break,
            Ok(CameraCommand::Arm) => {
                if !armed {
                    std::thread::sleep(MOCK_ARM_DELAY);
                    armed = true;
                    let _ = events.send(CameraEvent::Armed {
                        description: format!(
                            "{}x{} mock",
                            config.capture_resolution[0], config.capture_resolution[1]
                        ),
                        took: MOCK_ARM_DELAY,
                    });
                }
                continue;
            }
            Ok(CameraCommand::Disarm) => {
                armed = false;
                continue;
            }
            Ok(CameraCommand::Capture) => {
                if !armed {
                    tracing::warn!("mock capture from an unarmed camera; the shutter will lag");
                    std::thread::sleep(MOCK_ARM_DELAY);
                }
                let image = mock_frame(
                    config.capture_resolution[0],
                    config.capture_resolution[1],
                    started.elapsed().as_secs_f32(),
                );
                let _ = events.send(CameraEvent::Captured(Box::new(Capture {
                    image: orient(image, config),
                    taken_at: Instant::now(),
                })));
                continue;
            }
            Err(TryRecvError::Empty) => {}
        }
        // Armed cameras preview from the capture-resolution stream, which is
        // what keeps the picture moving right up to the shutter.
        let resolution = if armed {
            config.capture_resolution
        } else {
            config.preview_resolution
        };
        let image = mock_frame(
            resolution[0],
            resolution[1],
            started.elapsed().as_secs_f32(),
        );
        // Raw RGB through the same decoder thread a webcam's frames take.
        pending.offer(Buffer::new(
            Resolution::new(image.width(), image.height()),
            image.as_raw(),
            FrameFormat::RAWRGB,
        ));
        std::thread::sleep(MOCK_FRAME_INTERVAL);
    }
    Ok(())
}

fn mock_frame(width: u32, height: u32, seconds: f32) -> RgbImage {
    let sweep = ((seconds * 0.35).sin() * 0.5 + 0.5) * width as f32;
    RgbImage::from_fn(width.max(1), height.max(1), |x, y| {
        let distance = (x as f32 - sweep).abs() / width as f32;
        image::Rgb([
            (255.0 * (1.0 - distance)) as u8,
            (200.0 * y as f32 / height as f32) as u8,
            (200.0 * x as f32 / width as f32) as u8,
        ])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises whichever MJPEG decoder the current feature set selected.
    #[test]
    fn mjpeg_buffers_decode_to_the_source_image() {
        let source = image::RgbImage::from_fn(64, 32, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 8) as u8, 128])
        });
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode_image(&source)
            .unwrap();

        let buffer = Buffer::new(Resolution::new(64, 32), &jpeg, FrameFormat::MJPEG);
        let decoded = decode_buffer(&buffer).unwrap();

        assert_eq!((decoded.width(), decoded.height()), (64, 32));
        // JPEG is lossy, so compare loosely.
        for (expected, actual) in source.pixels().zip(decoded.pixels()) {
            for channel in 0..3 {
                let difference = i32::from(expected.0[channel]) - i32::from(actual.0[channel]);
                assert!(difference.abs() <= 12, "{expected:?} vs {actual:?}");
            }
        }
    }

    #[test]
    fn yuyv_gray_pixels_decode_to_gray() {
        // Y=128, U=V=128 is mid gray in BT.601.
        let bytes = vec![128u8, 128, 128, 128];
        let image = yuyv_to_rgb(&bytes, 2, 1).unwrap();
        for pixel in image.pixels() {
            for channel in pixel.0 {
                assert!((i32::from(channel) - 130).abs() <= 3, "got {channel}");
            }
        }
    }

    #[test]
    fn nv12_decodes_to_the_right_dimensions() {
        let bytes = vec![128u8; 4 * 4 + 4 * 4 / 2];
        let image = nv12_to_rgb(&bytes, 4, 4).unwrap();
        assert_eq!((image.width(), image.height()), (4, 4));
    }

    #[test]
    fn short_buffers_error_instead_of_panicking() {
        assert!(yuyv_to_rgb(&[0, 0], 4, 4).is_err());
        assert!(nv12_to_rgb(&[0, 0], 4, 4).is_err());
        assert!(gray_to_rgb(&[0, 0], 4, 4).is_err());
        assert!(raw_to_rgb(&[0, 0], 4, 4, false).is_err());
    }

    fn raw_buffer(value: u8) -> Buffer {
        Buffer::new(Resolution::new(1, 1), &[value; 3], FrameFormat::RAWRGB)
    }

    #[test]
    fn pending_frames_keep_only_the_newest() {
        let pending = PendingFrame::default();
        pending.offer(raw_buffer(1));
        pending.offer(raw_buffer(2));
        assert_eq!(pending.take().unwrap().buffer(), &[2, 2, 2]);
        pending.close();
        assert!(pending.take().is_none());
    }

    #[test]
    fn the_decoder_publishes_rotated_previews_until_closed() {
        let pending = PendingFrame::default();
        let preview_slot = Mutex::new(None);
        let config = config::Camera {
            rotation: 90,
            ..config::Camera::default()
        };
        std::thread::scope(|scope| {
            scope.spawn(|| decode_previews(&pending, &config, &preview_slot));
            pending.offer(Buffer::new(
                Resolution::new(4, 2),
                &[7; 4 * 2 * 3],
                FrameFormat::RAWRGB,
            ));
            let deadline = Instant::now() + Duration::from_secs(5);
            while preview_slot.lock().unwrap().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            pending.close();
        });
        let frame = preview_slot
            .lock()
            .unwrap()
            .take()
            .expect("a preview frame");
        assert_eq!((frame.image.width(), frame.image.height()), (2, 4));
        assert_eq!(frame.seq, 1);
    }

    #[test]
    fn capture_offsets_are_signed() {
        let shutter_at = Instant::now();
        let capture = |taken_at| Capture {
            image: RgbImage::new(1, 1),
            taken_at,
        };
        let after = capture(shutter_at + Duration::from_millis(40)).offset_ms(shutter_at);
        assert!((after - 40.0).abs() < 0.5, "{after}");
        let before = capture(shutter_at - Duration::from_millis(300)).offset_ms(shutter_at);
        assert!((before + 300.0).abs() < 0.5, "{before}");
    }

    #[test]
    fn mirroring_flips_after_rotating() {
        // Left pixel red, right pixel blue.
        let image = RgbImage::from_raw(2, 1, vec![255, 0, 0, 0, 0, 255]).unwrap();
        let mut config = config::Camera {
            mirror: true,
            ..config::Camera::default()
        };
        let mirrored = orient(image.clone(), &config);
        assert_eq!(mirrored.get_pixel(0, 0).0, [0, 0, 255]);

        // Rotated 90 degrees the pixels stack vertically, so mirroring has
        // nothing to swap.
        config.rotation = 90;
        let rotated = orient(image, &config);
        assert_eq!((rotated.width(), rotated.height()), (1, 2));
        assert_eq!(rotated.get_pixel(0, 0).0, [255, 0, 0]);
    }

    #[test]
    fn bgr_channels_are_swapped() {
        let image = raw_to_rgb(&[1, 2, 3], 1, 1, true).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [3, 2, 1]);
    }
}
