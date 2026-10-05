//! The booth UI: a full-screen egui app driving one state machine.
//!
//! ```text
//! Idle -> ModeSelect -> Countdown -> AwaitingCapture -> ShowCapture
//!                         ^                                |
//!                         +---- more shots in the queue ---+
//!                         |                                |
//!                      retakes                         queue empty
//!                         |                                |
//!                         +------------- Choose <----------+
//!                                          |
//!                                     looks good
//!                                          |
//!               Idle <- Review <- Processing
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ab_glyph::FontVec;
use anyhow::{Context, Result};
use eframe::egui;
use egui::{
    Align2, Color32, FontId, Rect, RichText, Stroke, StrokeKind, TextureHandle, TextureOptions,
    Vec2, pos2, vec2,
};
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

/// How long "Discard all" waits for its confirming second tap.
const DISCARD_CONFIRM_WINDOW: Duration = Duration::from_secs(4);

/// Frame pacing. The camera runs at 30fps; repainting a little faster keeps
/// the countdown smooth without spinning the GPU.
const REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// Texture coordinates covering a whole texture.
const FULL_UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

const BACKGROUND: Color32 = Color32::from_rgb(12, 12, 16);
const ACCENT: Color32 = Color32::from_rgb(255, 214, 102);
const DANGER: Color32 = Color32::from_rgb(255, 120, 120);

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
    /// All photos taken; the guests mark any to retake. Saves them as they are
    /// at `until`.
    Choose {
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
    /// One per photo taken so far, in sheet order. A retake replaces its slot.
    captures: Vec<RgbImage>,
    /// Screen-sized copies of `captures`, same order.
    capture_textures: Vec<TextureHandle>,
    /// Slots still to shoot in this round: every slot at first, then the
    /// retakes.
    shot_queue: VecDeque<u32>,
    /// Per slot, whether it is marked for retake on the choose screen.
    retake: Vec<bool>,
    /// Set by a first tap on "Discard all"; a second tap before then discards.
    discard_confirm_until: Option<Instant>,

    preview_texture: Option<TextureHandle>,
    preview_seq: u64,
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
            capture_textures: Vec::new(),
            shot_queue: VecDeque::new(),
            retake: Vec::new(),
            discard_confirm_until: None,
            preview_texture: None,
            preview_seq: 0,
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
        self.clear_session();
        self.sheet_texture = None;
        self.shot_queue = (0..self.mode().captures).collect();
        // Arm now, so the stream reconfiguration happens while the countdown
        // runs instead of at the shutter.
        self.camera.arm();
        self.next_shot(self.first_countdown());
    }

    /// Count down to the next queued shot, or move on to the choose screen
    /// once the queue is empty.
    fn next_shot(&mut self, countdown: Duration) {
        match self.shot_queue.pop_front() {
            Some(index) => {
                self.state = State::Countdown {
                    index,
                    ends_at: Instant::now() + countdown,
                };
            }
            None => self.choose_retakes(),
        }
    }

    /// Countdown before the first shot of a round, which is also the lead time
    /// the camera gets to arm.
    fn first_countdown(&self) -> Duration {
        Duration::from_secs(u64::from(self.config.countdown.seconds))
    }

    fn choose_retakes(&mut self) {
        // Nothing to shoot unless a retake is asked for; give the preview
        // format back meanwhile.
        self.camera.disarm();
        self.retake = vec![false; self.captures.len()];
        self.state = State::Choose {
            until: self.choice_deadline(),
        };
    }

    fn choice_deadline(&self) -> Instant {
        Instant::now()
            + Duration::from_secs_f32(self.config.countdown.retake_choice_seconds.max(0.0))
    }

    fn start_retakes(&mut self) {
        self.shot_queue = (0..)
            .zip(&self.retake)
            .filter(|(_, marked)| **marked)
            .map(|(index, _)| index)
            .collect();
        self.camera.arm();
        self.next_shot(self.first_countdown());
    }

    fn clear_session(&mut self) {
        self.captures.clear();
        self.capture_textures.clear();
        self.shot_queue.clear();
        self.retake.clear();
        self.discard_confirm_until = None;
    }

    /// "Discard all" on the choose screen: the first tap asks for confirmation,
    /// a second within [`DISCARD_CONFIRM_WINDOW`] drops every photo unsaved.
    fn press_discard(&mut self) {
        if self.discard_pending() {
            self.cancel_session();
        } else {
            self.discard_confirm_until = Some(Instant::now() + DISCARD_CONFIRM_WINDOW);
            self.state = State::Choose {
                until: self.choice_deadline(),
            };
        }
    }

    fn discard_pending(&self) -> bool {
        self.discard_confirm_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn cancel_session(&mut self) {
        self.clear_session();
        self.processing = None;
        self.camera.disarm();
        self.state = State::Idle;
    }

    fn fail(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::error!("{message}");
        self.clear_session();
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
        let slot = index as usize;
        let texture = upload_texture(
            ctx,
            self.capture_textures.get(slot).cloned(),
            "capture",
            &capture.preview,
        );
        if slot < self.captures.len() {
            self.captures[slot] = capture.image;
            self.capture_textures[slot] = texture;
        } else {
            self.captures.push(capture.image);
            self.capture_textures.push(texture);
        }
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
        self.clear_session();
        self.processing = Some(session::spawn_processing(ProcessRequest {
            mode,
            captures,
            output_dir: self.config.general.output_dir.clone(),
            font: self.font.clone(),
        }));
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
            State::ShowCapture { until, .. } if now >= until => {
                self.next_shot(Duration::from_secs_f32(
                    self.config.countdown.between_captures_seconds.max(0.0),
                ));
            }
            State::Choose { until } if now >= until => self.start_processing(),
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
            State::Choose { .. } => self.draw_choose(ui, rect),
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
            State::ShowCapture { .. }
                | State::Choose { .. }
                | State::Review { .. }
                | State::Processing
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
        if let Some(texture) = self.capture_textures.get(index as usize) {
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

    /// Every photo in a grid; tapping one toggles its retake mark. The button
    /// below reshoots the marked ones, or saves when none are marked.
    fn draw_choose(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let (area, button_rect) = review_layout(rect);
        let title_height = rect.height() * 0.09;
        ui.painter().text(
            pos2(rect.center().x, area.top() + title_height / 2.0),
            Align2::CENTER_CENTER,
            "Tap any photo to retake it",
            FontId::proportional(40.0),
            Color32::WHITE,
        );
        let grid_area = Rect::from_min_max(pos2(area.left(), area.top() + title_height), area.max);
        let aspect = self
            .capture_textures
            .first()
            .map_or(16.0 / 9.0, TextureHandle::aspect_ratio);
        let tiles = tile_grid(
            grid_area,
            self.capture_textures.len(),
            aspect,
            rect.height() * 0.02,
        );

        let mut toggled = None;
        for (index, (tile, texture)) in tiles.iter().zip(&self.capture_textures).enumerate() {
            let target = fit(*tile, texture.size_vec2());
            let painter = ui.painter();
            painter.image(texture.id(), target, FULL_UV, Color32::WHITE);
            let badge = target.left_top() + vec2(28.0, 28.0);
            painter.circle_filled(badge, 20.0, Color32::from_black_alpha(160));
            painter.text(
                badge,
                Align2::CENTER_CENTER,
                format!("{}", index + 1),
                FontId::proportional(26.0),
                Color32::WHITE,
            );
            if self.retake.get(index).copied().unwrap_or(false) {
                painter.rect_filled(target, 0.0, Color32::from_black_alpha(140));
                painter.rect_stroke(target, 0.0, Stroke::new(6.0, ACCENT), StrokeKind::Inside);
                painter.text(
                    target.center(),
                    Align2::CENTER_CENTER,
                    "Retake",
                    FontId::proportional(40.0),
                    ACCENT,
                );
            }
            let id = ui.id().with(("retake", index));
            if ui.interact(target, id, egui::Sense::click()).clicked() {
                toggled = Some(index);
            }
        }
        if let Some(marked) = toggled.and_then(|index| self.retake.get_mut(index)) {
            *marked = !*marked;
            // Still deciding: keep the screen up.
            self.state = State::Choose {
                until: self.choice_deadline(),
            };
        }

        let discard_rect = Rect::from_min_size(
            pos2(rect.left() + rect.height() * 0.03, button_rect.top()),
            Vec2::new(rect.width() * 0.25, button_rect.height()),
        );
        let discard_label = if self.discard_pending() {
            "Tap again to discard"
        } else {
            "Discard all"
        };
        if ui
            .put(
                discard_rect,
                egui::Button::new(RichText::new(discard_label).size(26.0).color(DANGER)),
            )
            .clicked()
        {
            self.press_discard();
            return;
        }

        let marked = self.retake.iter().filter(|marked| **marked).count();
        let label = match marked {
            0 => "Looks good".to_string(),
            1 => "Retake 1 photo".to_string(),
            count => format!("Retake {count} photos"),
        };
        if ui
            .put(
                button_rect,
                egui::Button::new(RichText::new(label).size(36.0)),
            )
            .clicked()
        {
            if marked == 0 {
                self.start_processing();
            } else {
                self.start_retakes();
            }
        }
    }

    fn draw_review(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let (sheet_area, done_rect) = review_layout(rect);
        if let Some(texture) = &self.sheet_texture {
            let target = fit(sheet_area, texture.size_vec2());
            ui.painter()
                .image(texture.id(), target, FULL_UV, Color32::WHITE);
        }
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
            DANGER,
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

/// Review screen: the area the sheet is fitted into, and the Done button in
/// its own band below it, so the button never covers the sheet's footer.
fn review_layout(rect: Rect) -> (Rect, Rect) {
    let margin = rect.height() * 0.03;
    let button_size = Vec2::new(rect.width() * 0.3, rect.height() * 0.09);
    let done_rect = Rect::from_center_size(
        pos2(
            rect.center().x,
            rect.bottom() - margin - button_size.y / 2.0,
        ),
        button_size,
    );
    let sheet_area = Rect::from_min_max(
        rect.min + Vec2::splat(margin),
        pos2(rect.right() - margin, done_rect.top() - margin),
    );
    (sheet_area, done_rect)
}

/// Rects for `count` tiles of `aspect` (width / height) in `area`, using the
/// column count that makes the tiles largest. Each row is centred.
fn tile_grid(area: Rect, count: usize, aspect: f32, gap: f32) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let tile_size = |columns: usize| {
        let rows = count.div_ceil(columns);
        let width = (area.width() - gap * (columns - 1) as f32) / columns as f32;
        let height = (area.height() - gap * (rows - 1) as f32) / rows as f32;
        let width = width.min(height * aspect).max(0.0);
        Vec2::new(width, width / aspect)
    };
    let columns = (1..=count)
        .max_by(|&a, &b| tile_size(a).x.total_cmp(&tile_size(b).x))
        .unwrap_or(1);
    let size = tile_size(columns);
    let rows = count.div_ceil(columns);
    let grid_height = size.y * rows as f32 + gap * (rows - 1) as f32;
    let top = area.center().y - grid_height / 2.0;
    (0..count)
        .map(|index| {
            let (row, column) = (index / columns, index % columns);
            let in_row = (count - row * columns).min(columns);
            let row_width = size.x * in_row as f32 + gap * (in_row - 1) as f32;
            let left = area.center().x - row_width / 2.0;
            Rect::from_min_size(
                pos2(
                    left + column as f32 * (size.x + gap),
                    top + row as f32 * (size.y + gap),
                ),
                size,
            )
        })
        .collect()
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
    use crate::config::{CameraBackend, DEFAULT_CONFIG_TOML};

    /// Drive the state machine the way `ui` does, minus drawing, until `done`.
    fn run_until(app: &mut BoothApp, ctx: &egui::Context, done: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done(&app.state) {
            assert!(Instant::now() < deadline, "stuck in {:?}", app.state);
            app.drain_camera_events(ctx);
            app.poll_processing(ctx);
            app.advance();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn retakes_replace_only_the_marked_photos() {
        let output_dir = std::env::temp_dir().join(format!(
            "rsbooth-ui-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut config: Config = toml::from_str(DEFAULT_CONFIG_TOML).unwrap();
        config.general.output_dir = output_dir.clone();
        config.general.font = None;
        config.camera.backend = CameraBackend::Mock;
        config.camera.preview_resolution = [160, 90];
        config.camera.capture_resolution = [320, 180];
        config.countdown.seconds = 0;
        config.countdown.between_captures_seconds = 0.0;
        config.countdown.capture_review_seconds = 0.0;
        config.modes[0].sheet = [400, 1200];
        config.modes[0].footer_height = 100;
        let shots = config.modes[0].captures as usize;

        let ctx = egui::Context::default();
        let mut app = BoothApp::new(config).unwrap();
        app.start_session(0);
        run_until(&mut app, &ctx, |state| {
            matches!(state, State::Choose { .. })
        });
        assert_eq!(app.captures.len(), shots);
        assert_eq!(app.capture_textures.len(), shots);

        // The mock's picture drifts over time, so a retake differs from the
        // photo it replaces.
        let originals = app.captures.clone();
        app.retake[1] = true;
        app.start_retakes();
        run_until(&mut app, &ctx, |state| {
            matches!(state, State::Choose { .. })
        });
        assert_eq!(app.captures.len(), shots);
        assert_ne!(app.captures[1], originals[1]);
        for index in (0..shots).filter(|&index| index != 1) {
            assert_eq!(app.captures[index], originals[index], "photo {index}");
        }
        assert!(app.retake.iter().all(|marked| !marked));

        // One tap on "Discard all" only asks; nothing is dropped yet.
        app.press_discard();
        assert!(matches!(app.state, State::Choose { .. }));
        assert_eq!(app.captures.len(), shots);

        app.start_processing();
        run_until(&mut app, &ctx, |state| {
            matches!(state, State::Review { .. })
        });
        let session_dir = app.last_session_dir.clone().unwrap();
        assert!(session_dir.join("sheet.jpg").exists());
        assert!(session_dir.join(format!("capture_{shots}.jpg")).exists());
        assert!(
            !session_dir
                .join(format!("capture_{}.jpg", shots + 1))
                .exists()
        );

        std::fs::remove_dir_all(&output_dir).ok();
    }

    #[test]
    fn the_done_button_sits_below_the_sheet() {
        let screen = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(1280.0, 800.0));
        let (sheet_area, done_rect) = review_layout(screen);
        assert!(sheet_area.bottom() < done_rect.top());
        assert!(screen.contains_rect(sheet_area) && screen.contains_rect(done_rect));
        // A tall strip fills the sheet area's height and still clears the button.
        let strip = fit(sheet_area, Vec2::new(1200.0, 3600.0));
        assert!(strip.bottom() <= done_rect.top());
    }

    #[test]
    fn discard_needs_a_second_tap() {
        let mut config: Config = toml::from_str(DEFAULT_CONFIG_TOML).unwrap();
        config.general.output_dir = std::env::temp_dir();
        config.camera.backend = CameraBackend::Mock;
        let mut app = BoothApp::new(config).unwrap();
        app.captures = vec![RgbImage::new(4, 4); 2];
        app.retake = vec![false; 2];
        app.state = State::Choose {
            until: Instant::now() + Duration::from_secs(30),
        };

        app.press_discard();
        assert!(matches!(app.state, State::Choose { .. }));
        assert_eq!(app.captures.len(), 2);

        app.press_discard();
        assert!(matches!(app.state, State::Idle));
        assert!(app.captures.is_empty());
    }

    #[test]
    fn four_wide_photos_tile_two_by_two() {
        let area = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(1280.0, 600.0));
        let tiles = tile_grid(area, 4, 16.0 / 9.0, 10.0);
        assert_eq!(tiles.len(), 4);
        assert_eq!(tiles[0].top(), tiles[1].top());
        assert!(tiles[2].top() > tiles[0].bottom());
        for (index, tile) in tiles.iter().enumerate() {
            assert!(area.contains_rect(*tile), "{tile:?}");
            for other in &tiles[index + 1..] {
                assert!(!tile.intersects(*other), "{tile:?} overlaps {other:?}");
            }
        }
    }

    #[test]
    fn a_short_last_row_is_centred() {
        let area = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(1000.0, 1000.0));
        let tiles = tile_grid(area, 3, 1.0, 0.0);
        assert!((tiles[2].center().x - area.center().x).abs() < 0.01);
        assert!(tile_grid(area, 0, 1.0, 0.0).is_empty());
    }

    #[test]
    fn fit_preserves_the_aspect_ratio() {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 600.0));
        let fitted = fit(outer, Vec2::new(1920.0, 1080.0));
        assert!((fitted.width() - 800.0).abs() < 0.01);
        assert!((fitted.height() - 450.0).abs() < 0.01);
        assert_eq!(fitted.center(), outer.center());
    }
}
