//! The booth UI: a full-screen egui app driving one state machine.
//!
//! ```text
//! Idle -> ModeSelect -> Countdown -> AwaitingCapture -> ShowCapture
//!            ^                            |                 |
//!            |                            +--- more shots --+
//!            |                                              |
//!          Review <- Processing <-------- last shot --------+
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ab_glyph::FontVec;
use anyhow::{Context, Result};
use eframe::egui;
use egui::{Align2, Color32, FontId, Rect, RichText, TextureHandle, TextureOptions, Vec2, pos2};
use image::RgbImage;

use crate::camera::{CameraEvent, CameraHandle, Capture};
use crate::config::{Config, Mode};
use crate::session::{self, ProcessRequest, Processed};

/// How long the white capture flash lasts.
const FLASH_DURATION: Duration = Duration::from_millis(220);

/// Gap either side of the countdown hitting zero and the stored frame being
/// taken, past which the booth says so on screen. Anything visible here means
/// the photo no longer matches the pose the countdown asked for.
const MAX_ACCEPTABLE_SHUTTER_LAG: Duration = Duration::from_millis(250);

/// How long after the shutter the booth waits for the camera before giving up
/// on the session. Generous enough for an unarmed capture's in-line format
/// switch.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// Status lines kept for the corner log.
const STATUS_LINES: usize = 4;

/// How long an error stays on screen before the booth returns to idle.
const ERROR_DISPLAY: Duration = Duration::from_secs(8);

/// How long the mode-select screen waits for a choice before returning to
/// idle, so a walk-away does not leave the camera armed.
const MODE_SELECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Frame pacing. The camera runs at 30fps; repainting a little faster keeps
/// the countdown smooth without spinning the GPU.
const REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// Texture coordinates covering a whole texture.
const FULL_UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

const BACKGROUND: Color32 = Color32::from_rgb(12, 12, 16);
const ACCENT: Color32 = Color32::from_rgb(255, 214, 102);

#[derive(Debug)]
enum State {
    Idle,
    ModeSelect {
        until: Instant,
    },
    /// Counting down to capture `index` (0-based).
    Countdown {
        index: u32,
        ends_at: Instant,
    },
    /// Shutter requested; waiting for the camera thread. `shutter_at` is the
    /// instant the countdown hit zero, which the stored frame's timestamp is
    /// measured against.
    AwaitingCapture {
        index: u32,
        shutter_at: Instant,
    },
    /// Showing the capture that was just taken.
    ShowCapture {
        index: u32,
        until: Instant,
    },
    /// Composing and saving on the worker thread.
    Processing,
    /// Showing the finished sheet.
    Review {
        until: Instant,
    },
    Error {
        message: String,
        until: Instant,
    },
}

pub struct BoothApp {
    config: Config,
    camera: CameraHandle,
    font: Option<Arc<FontVec>>,
    state: State,

    /// Index into `config.modes` for the session in progress.
    active_mode: usize,
    captures: Vec<RgbImage>,

    preview_texture: Option<TextureHandle>,
    preview_seq: u64,
    capture_texture: Option<TextureHandle>,
    sheet_texture: Option<TextureHandle>,

    processing: Option<Receiver<Result<Processed>>>,
    last_session_dir: Option<PathBuf>,

    camera_status: String,
    camera_fatal: Option<String>,
    status_lines: VecDeque<String>,
    flash_until: Option<Instant>,
}

impl BoothApp {
    pub fn new(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.general.output_dir)
            .with_context(|| format!("creating {}", config.general.output_dir.display()))?;
        let camera = CameraHandle::spawn(config.camera.clone());
        let font = session::load_font_or_warn(config.general.font.as_deref());
        Ok(Self {
            config,
            camera,
            font,
            state: State::Idle,
            active_mode: 0,
            captures: Vec::new(),
            preview_texture: None,
            preview_seq: 0,
            capture_texture: None,
            sheet_texture: None,
            processing: None,
            last_session_dir: None,
            camera_status: "starting the camera...".to_string(),
            camera_fatal: None,
            status_lines: VecDeque::new(),
            flash_until: None,
        })
    }

    fn mode(&self) -> &Mode {
        &self.config.modes[self.active_mode.min(self.config.modes.len() - 1)]
    }

    fn push_status(&mut self, line: impl Into<String>) {
        let line = line.into();
        tracing::info!("{line}");
        self.status_lines.push_back(line);
        while self.status_lines.len() > STATUS_LINES {
            self.status_lines.pop_front();
        }
    }

    fn start_session(&mut self, mode_index: usize) {
        self.active_mode = mode_index;
        self.captures.clear();
        self.capture_texture = None;
        self.sheet_texture = None;
        // Arm now, so the stream reconfiguration happens while the countdown
        // runs instead of at the shutter.
        self.camera.arm();
        self.state = State::Countdown {
            index: 0,
            ends_at: Instant::now() + Duration::from_secs(u64::from(self.config.countdown.seconds)),
        };
    }

    fn cancel_session(&mut self) {
        self.captures.clear();
        self.processing = None;
        self.camera.disarm();
        self.state = State::Idle;
    }

    fn fail(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::error!("{message}");
        self.captures.clear();
        self.camera.disarm();
        self.state = State::Error {
            message,
            until: Instant::now() + ERROR_DISPLAY,
        };
    }

    /// Called when the user asks to start: skips mode selection when there is
    /// only one mode to pick.
    fn begin(&mut self) {
        if let Some(error) = &self.camera_fatal {
            // A countdown would end on a shutter nothing can answer.
            self.fail(format!("No camera: {error}"));
        } else if self.config.modes.len() == 1 {
            self.start_session(0);
        } else {
            // Arm while the user reads the mode buttons: free lead time.
            self.camera.arm();
            self.state = State::ModeSelect {
                until: Instant::now() + MODE_SELECT_TIMEOUT,
            };
        }
    }

    fn drain_camera_events(&mut self, ctx: &egui::Context) {
        for event in self.camera.poll_events() {
            match event {
                CameraEvent::Ready(description) => {
                    self.camera_status = description.clone();
                    self.push_status(format!("camera: {description}"));
                }
                CameraEvent::Armed { description, took } => {
                    tracing::info!("camera armed at {description} in {took:?}");
                }
                CameraEvent::Captured(capture) => self.on_capture(*capture, ctx),
                CameraEvent::CaptureFailed(error) => {
                    self.fail(format!("Capture failed: {error}"));
                }
                CameraEvent::Warning(warning) => self.push_status(warning),
                CameraEvent::Fatal(error) => {
                    self.camera_fatal = Some(error.clone());
                    // Drop the last frame so the backdrop says why it stopped.
                    self.preview_texture = None;
                    self.fail(format!("Camera stopped: {error}"));
                }
            }
        }
    }

    /// Freeze the screen on the frame that was actually stored.
    ///
    /// The picture stays live right up to this point, so the moment the image
    /// stops moving is the moment the stored photo was taken - never a stale
    /// preview frame standing in for a shutter that has not fired yet.
    fn on_capture(&mut self, capture: Capture, ctx: &egui::Context) {
        let State::AwaitingCapture { index, shutter_at } = self.state else {
            tracing::warn!("ignoring a capture that arrived outside a session");
            return;
        };
        let offset_ms = capture.offset_ms(shutter_at);
        tracing::info!("shutter lag: {offset_ms:+.0}ms");
        if offset_ms.abs() > MAX_ACCEPTABLE_SHUTTER_LAG.as_secs_f32() * 1000.0 {
            self.push_status(format!(
                "photo taken {offset_ms:+.0}ms from the end of the countdown"
            ));
        }
        self.capture_texture = Some(upload_texture(
            ctx,
            self.capture_texture.take(),
            "capture",
            &capture.preview,
        ));
        self.captures.push(capture.image);
        if self.config.window.flash {
            self.flash_until = Some(Instant::now() + FLASH_DURATION);
        }
        self.state = State::ShowCapture {
            index,
            until: Instant::now()
                + Duration::from_secs_f32(self.config.countdown.capture_review_seconds.max(0.0)),
        };
    }

    fn poll_processing(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.processing else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("the processing thread died")),
        };
        self.processing = None;
        match result {
            Ok(processed) => {
                self.sheet_texture = Some(upload_texture(
                    ctx,
                    self.sheet_texture.take(),
                    "sheet",
                    &processed.preview,
                ));
                self.push_status(format!(
                    "saved {} photo(s) to {}",
                    processed.capture_paths.len() + 1,
                    processed.directory.display()
                ));
                self.last_session_dir = Some(processed.directory.clone());
                self.state = State::Review {
                    until: Instant::now()
                        + Duration::from_secs_f32(self.config.countdown.review_seconds.max(0.0)),
                };
            }
            Err(error) => self.fail(format!("Could not build the photo sheet: {error:#}")),
        }
    }

    fn start_processing(&mut self) {
        let mode = self.mode().clone();
        let captures = std::mem::take(&mut self.captures);
        self.processing = Some(session::spawn_processing(ProcessRequest {
            mode,
            captures,
            output_dir: self.config.general.output_dir.clone(),
            font: self.font.clone(),
        }));
        // The session is done shooting; give the preview format back.
        self.camera.disarm();
        self.state = State::Processing;
    }

    /// Timer-driven transitions.
    fn advance(&mut self) {
        let now = Instant::now();
        match self.state {
            State::Countdown { index, ends_at } if now >= ends_at => {
                self.camera.request_capture();
                self.state = State::AwaitingCapture {
                    index,
                    shutter_at: ends_at,
                };
            }
            State::AwaitingCapture { shutter_at, .. } if now >= shutter_at + CAPTURE_TIMEOUT => {
                self.fail("The camera did not return a photo");
            }
            State::ShowCapture { index, until } if now >= until => {
                if index + 1 < self.mode().captures {
                    self.state = State::Countdown {
                        index: index + 1,
                        ends_at: now
                            + Duration::from_secs_f32(
                                self.config.countdown.between_captures_seconds.max(0.0),
                            ),
                    };
                } else {
                    self.start_processing();
                }
            }
            State::ModeSelect { until } if now >= until => self.cancel_session(),
            State::Review { until } if now >= until => self.state = State::Idle,
            State::Error { until, .. } if now >= until => self.state = State::Idle,
            _ => {}
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (start, cancel) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::Space) || input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Escape),
            )
        });
        // Esc backs out one level: a session to idle, idle to the desktop.
        if cancel {
            match self.state {
                State::Idle => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                _ => self.cancel_session(),
            }
        }
        if start && matches!(self.state, State::Idle) {
            self.begin();
        }
    }

    fn update_preview_texture(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.camera.take_preview(self.preview_seq) else {
            return;
        };
        self.preview_seq = frame.seq;
        self.preview_texture = Some(upload_texture(
            ctx,
            self.preview_texture.take(),
            "preview",
            &frame.image,
        ));
    }
}

impl eframe::App for BoothApp {
    /// eframe hands us a margin-free root `Ui`, which is exactly what a
    /// full-bleed booth wants: everything is painted edge to edge.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_camera_events(&ctx);
        self.poll_processing(&ctx);
        self.update_preview_texture(&ctx);
        self.handle_keys(&ctx);
        self.advance();

        let rect = ui.max_rect();
        self.draw_backdrop(ui, rect);
        match &self.state {
            State::Idle => self.draw_idle(ui, rect),
            State::ModeSelect { .. } => self.draw_mode_select(ui, rect),
            State::Countdown { index, ends_at } => {
                let (index, ends_at) = (*index, *ends_at);
                self.draw_countdown(ui, rect, index, ends_at);
            }
            State::AwaitingCapture { .. } => {
                centered_text(ui, rect, "Hold still", 96.0, ACCENT);
            }
            State::ShowCapture { index, .. } => {
                let index = *index;
                self.draw_show_capture(ui, rect, index);
            }
            State::Processing => {
                centered_text(ui, rect, "Making your photos...", 72.0, Color32::WHITE);
            }
            State::Review { .. } => self.draw_review(ui, rect),
            State::Error { message, .. } => {
                let message = message.clone();
                self.draw_error(ui, rect, &message);
            }
        }
        self.draw_flash(ui, rect);
        self.draw_status(ui, rect);
        if self.config.window.hide_cursor {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }

        ctx.request_repaint_after(REPAINT_INTERVAL);
    }
}

/// Drawing.
impl BoothApp {
    /// Live preview, or a message when there is no camera.
    fn draw_backdrop(&self, ui: &egui::Ui, rect: Rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, BACKGROUND);
        let show_preview = !matches!(
            self.state,
            State::ShowCapture { .. } | State::Review { .. } | State::Processing
        );
        if !show_preview {
            return;
        }
        match &self.preview_texture {
            Some(texture) => {
                let target = fit(rect, texture.size_vec2());
                painter.image(texture.id(), target, FULL_UV, Color32::WHITE);
            }
            None => {
                let message = match &self.camera_fatal {
                    Some(error) => format!("No camera: {error}"),
                    None => "Waiting for the camera...".to_string(),
                };
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    message,
                    FontId::proportional(32.0),
                    Color32::GRAY,
                );
            }
        }
    }

    fn draw_idle(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let button_rect = Rect::from_center_size(
            rect.center(),
            Vec2::new(rect.width() * 0.5, rect.height() * 0.2),
        );
        let text = self.config.general.idle_text.clone();
        if ui
            .put(
                button_rect,
                egui::Button::new(RichText::new(text).size(64.0)),
            )
            .clicked()
        {
            self.begin();
        }
    }

    fn draw_mode_select(&mut self, ui: &mut egui::Ui, rect: Rect) {
        scrim(ui, rect);
        ui.painter().text(
            pos2(rect.center().x, rect.top() + rect.height() * 0.12),
            Align2::CENTER_CENTER,
            "Choose a layout",
            FontId::proportional(56.0),
            Color32::WHITE,
        );

        let count = self.config.modes.len().max(1);
        let button_height = (rect.height() * 0.6 / count as f32).min(140.0);
        let top = rect.top() + rect.height() * 0.22;
        let mut chosen = None;
        for (index, mode) in self.config.modes.iter().enumerate() {
            let button_rect = Rect::from_center_size(
                pos2(
                    rect.center().x,
                    top + button_height * (index as f32 + 0.5) + 12.0 * index as f32,
                ),
                Vec2::new(rect.width() * 0.6, button_height * 0.9),
            );
            let label = if mode.description.is_empty() {
                format!("{} - {} shots", mode.name, mode.captures)
            } else {
                format!("{}\n{}", mode.name, mode.description)
            };
            if ui
                .put(
                    button_rect,
                    egui::Button::new(RichText::new(label).size(32.0)),
                )
                .clicked()
            {
                chosen = Some(index);
            }
        }
        if let Some(index) = chosen {
            self.start_session(index);
        }
        if self.cancel_button(ui, rect).clicked() {
            self.cancel_session();
        }
    }

    fn draw_countdown(&mut self, ui: &mut egui::Ui, rect: Rect, index: u32, ends_at: Instant) {
        let remaining = ends_at
            .saturating_duration_since(Instant::now())
            .as_secs_f32();
        let label = if remaining <= 0.2 {
            "Smile!".to_string()
        } else {
            format!("{}", remaining.ceil() as u32)
        };
        // Scale the digit up as it approaches zero for a bit of urgency.
        let pulse = 1.0 + (1.0 - remaining.fract()) * 0.15;
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional((rect.height() * 0.35 * pulse).min(420.0)),
            ACCENT,
        );
        ui.painter().text(
            pos2(rect.center().x, rect.bottom() - rect.height() * 0.1),
            Align2::CENTER_CENTER,
            format!("Photo {} of {}", index + 1, self.mode().captures),
            FontId::proportional(36.0),
            Color32::WHITE,
        );
        if self.cancel_button(ui, rect).clicked() {
            self.cancel_session();
        }
    }

    fn draw_show_capture(&self, ui: &egui::Ui, rect: Rect, index: u32) {
        if let Some(texture) = &self.capture_texture {
            let target = fit(rect, texture.size_vec2());
            ui.painter()
                .image(texture.id(), target, FULL_UV, Color32::WHITE);
        }
        ui.painter().text(
            pos2(rect.center().x, rect.bottom() - rect.height() * 0.08),
            Align2::CENTER_CENTER,
            format!("{} of {}", index + 1, self.mode().captures),
            FontId::proportional(40.0),
            ACCENT,
        );
    }

    fn draw_review(&mut self, ui: &mut egui::Ui, rect: Rect) {
        if let Some(texture) = &self.sheet_texture {
            let target = fit(rect.shrink(rect.height() * 0.06), texture.size_vec2());
            ui.painter()
                .image(texture.id(), target, FULL_UV, Color32::WHITE);
        }
        let done_rect = Rect::from_center_size(
            pos2(rect.center().x, rect.bottom() - rect.height() * 0.06),
            Vec2::new(rect.width() * 0.3, rect.height() * 0.09),
        );
        if ui
            .put(
                done_rect,
                egui::Button::new(RichText::new("Done").size(36.0)),
            )
            .clicked()
        {
            self.state = State::Idle;
        }
    }

    fn draw_error(&mut self, ui: &mut egui::Ui, rect: Rect, message: &str) {
        scrim(ui, rect);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            message,
            FontId::proportional(36.0),
            Color32::from_rgb(255, 120, 120),
        );
        let ok_rect = Rect::from_center_size(
            pos2(rect.center().x, rect.center().y + rect.height() * 0.2),
            Vec2::new(rect.width() * 0.2, rect.height() * 0.08),
        );
        if ui
            .put(ok_rect, egui::Button::new(RichText::new("OK").size(32.0)))
            .clicked()
        {
            self.state = State::Idle;
        }
    }

    fn draw_flash(&self, ui: &egui::Ui, rect: Rect) {
        let Some(until) = self.flash_until else {
            return;
        };
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        let alpha = (remaining.as_secs_f32() / FLASH_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        ui.painter()
            .rect_filled(rect, 0.0, Color32::from_white_alpha((alpha * 220.0) as u8));
    }

    fn draw_status(&self, ui: &egui::Ui, rect: Rect) {
        let mut y = rect.bottom() - 8.0;
        for line in self.status_lines.iter().rev() {
            ui.painter().text(
                pos2(rect.left() + 12.0, y),
                Align2::LEFT_BOTTOM,
                line,
                FontId::proportional(16.0),
                Color32::from_gray(150),
            );
            y -= 20.0;
        }
    }

    fn cancel_button(&self, ui: &mut egui::Ui, rect: Rect) -> egui::Response {
        let cancel_rect = Rect::from_min_size(
            pos2(rect.right() - 150.0, rect.top() + 16.0),
            Vec2::new(130.0, 52.0),
        );
        ui.put(
            cancel_rect,
            egui::Button::new(RichText::new("Cancel").size(22.0)),
        )
    }
}

/// Upload or replace a texture, reusing the existing handle when the size
/// matches so the GPU allocation is kept.
fn upload_texture(
    ctx: &egui::Context,
    existing: Option<TextureHandle>,
    name: &str,
    image: &RgbImage,
) -> TextureHandle {
    let size = [image.width() as usize, image.height() as usize];
    let color_image = egui::ColorImage::from_rgb(size, image.as_raw());
    match existing {
        Some(mut texture) if texture.size() == size => {
            texture.set(color_image, TextureOptions::LINEAR);
            texture
        }
        _ => ctx.load_texture(name, color_image, TextureOptions::LINEAR),
    }
}

/// Largest rect with `content` aspect ratio that fits inside `outer`, centred.
fn fit(outer: Rect, content: Vec2) -> Rect {
    if content.x <= 0.0 || content.y <= 0.0 {
        return outer;
    }
    let scale = (outer.width() / content.x).min(outer.height() / content.y);
    Rect::from_center_size(outer.center(), content * scale)
}

fn scrim(ui: &egui::Ui, rect: Rect) {
    ui.painter()
        .rect_filled(rect, 0.0, Color32::from_black_alpha(170));
}

fn centered_text(ui: &egui::Ui, rect: Rect, text: &str, size: f32, color: Color32) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(size),
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_preserves_the_aspect_ratio() {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 600.0));
        let fitted = fit(outer, Vec2::new(1920.0, 1080.0));
        assert!((fitted.width() - 800.0).abs() < 0.01);
        assert!((fitted.height() - 450.0).abs() < 0.01);
        assert_eq!(fitted.center(), outer.center());
    }
}
