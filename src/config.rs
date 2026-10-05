//! Configuration: TOML file -> typed structs.
//!
//! Everything the booth does at runtime - layouts, countdown timings, camera
//! formats, where photos are written - comes from here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// Name of the config file looked up in the working directory / the config dir.
pub const CONFIG_FILE_NAME: &str = "rsbooth.toml";

/// Directory under the user's config dir holding the config file.
pub const APP_DIR_NAME: &str = "rsbooth";

/// Config shipped with the binary, written out by `rsbooth init-config`.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../rsbooth.toml");

/// Log file name under `output_dir` when `general.log_file` is unset.
pub const DEFAULT_LOG_FILE_NAME: &str = "rsbooth.log";

/// Frames pulled and thrown away right after the camera is armed, so
/// auto-exposure and auto-white-balance settle during the countdown rather
/// than at the shutter.
pub const DEFAULT_WARMUP_FRAMES: u32 = 4;

/// Frames dropped at the shutter before the one that is kept.
pub const DEFAULT_CAPTURE_DISCARD_FRAMES: u32 = 1;

/// Per-user directories, resolved once in `main` (they come from `$HOME` and
/// `$XDG_CONFIG_HOME`, or the platform equivalents).
pub struct UserDirs {
    pub home: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub window: Window,
    #[serde(default)]
    pub camera: Camera,
    #[serde(default)]
    pub countdown: Countdown,
    #[serde(default)]
    pub modes: Vec<Mode>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    /// Where sessions are written. One subdirectory per session.
    #[serde(default = "General::default_output_dir")]
    pub output_dir: PathBuf,
    /// Text shown on the idle/attract screen.
    #[serde(default = "General::default_idle_text")]
    pub idle_text: String,
    /// TTF/OTF used for footer text drawn into the sheet. Footer text is
    /// skipped (with a warning) when unset or unreadable.
    #[serde(default)]
    pub font: Option<PathBuf>,
    /// Appended to with everything logged: camera details, warnings, the files
    /// each session wrote. Defaults to `<output_dir>/rsbooth.log`.
    #[serde(default)]
    pub log_file: Option<PathBuf>,
}

impl General {
    fn default_output_dir() -> PathBuf {
        PathBuf::from("~/Pictures/rsbooth")
    }
    fn default_idle_text() -> String {
        "Touch to start".to_string()
    }

    pub fn log_path(&self) -> PathBuf {
        self.log_file
            .clone()
            .unwrap_or_else(|| self.output_dir.join(DEFAULT_LOG_FILE_NAME))
    }
}

impl Default for General {
    fn default() -> Self {
        Self {
            output_dir: Self::default_output_dir(),
            idle_text: Self::default_idle_text(),
            font: None,
            log_file: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    #[serde(default)]
    pub fullscreen: bool,
    /// Hide the mouse pointer over the booth, for touchscreen kiosks.
    #[serde(default)]
    pub hide_cursor: bool,
    #[serde(default = "Window::default_size")]
    pub size: [f32; 2],
    /// White flash overlay at the moment of capture.
    #[serde(default = "default_true")]
    pub flash: bool,
}

impl Window {
    fn default_size() -> [f32; 2] {
        [1280.0, 800.0]
    }
}

impl Default for Window {
    fn default() -> Self {
        Self {
            fullscreen: false,
            hide_cursor: false,
            size: Self::default_size(),
            flash: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraBackend {
    /// USB / UVC camera through nokhwa (V4L2, MediaFoundation, AVFoundation).
    Webcam,
    /// Synthetic frames. Lets the whole app run with no camera attached.
    Mock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormatPref {
    Mjpeg,
    Yuyv,
    Any,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    #[serde(default = "Camera::default_backend")]
    pub backend: CameraBackend,
    /// Camera index as reported by `rsbooth list-cameras`.
    #[serde(default)]
    pub index: u32,
    #[serde(default = "Camera::default_preview_resolution")]
    pub preview_resolution: [u32; 2],
    /// Resolution used for the kept still. When it differs from the preview
    /// resolution the stream is reconfigured around each capture.
    #[serde(default = "Camera::default_capture_resolution")]
    pub capture_resolution: [u32; 2],
    #[serde(default = "Camera::default_format")]
    pub format: PixelFormatPref,
    #[serde(default = "Camera::default_warmup_frames")]
    pub warmup_frames: u32,
    /// Frames dropped at the shutter before the one that is kept. The preview
    /// keeps the driver's buffer queue drained, so 1 is enough to guarantee the
    /// kept frame was exposed after the countdown reached zero.
    #[serde(default = "Camera::default_discard_frames")]
    pub capture_discard_frames: u32,
    /// Rotate the preview and the photos clockwise: 0, 90, 180 or 270.
    #[serde(default)]
    pub rotation: u32,
    /// Flip the preview and the photos left to right, as in a mirror, after
    /// rotating. The saved files match what was on screen.
    #[serde(default = "default_true")]
    pub mirror: bool,
}

impl Camera {
    fn default_backend() -> CameraBackend {
        CameraBackend::Webcam
    }
    fn default_preview_resolution() -> [u32; 2] {
        [1280, 720]
    }
    fn default_capture_resolution() -> [u32; 2] {
        [1920, 1080]
    }
    fn default_format() -> PixelFormatPref {
        PixelFormatPref::Any
    }
    fn default_warmup_frames() -> u32 {
        DEFAULT_WARMUP_FRAMES
    }
    fn default_discard_frames() -> u32 {
        DEFAULT_CAPTURE_DISCARD_FRAMES
    }
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            backend: Self::default_backend(),
            index: 0,
            preview_resolution: Self::default_preview_resolution(),
            capture_resolution: Self::default_capture_resolution(),
            format: Self::default_format(),
            warmup_frames: Self::default_warmup_frames(),
            capture_discard_frames: Self::default_discard_frames(),
            rotation: 0,
            mirror: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Countdown {
    /// Seconds counted down before the first capture.
    #[serde(default = "Countdown::default_seconds")]
    pub seconds: u32,
    /// Pause between captures within one session.
    #[serde(default = "Countdown::default_between")]
    pub between_captures_seconds: f32,
    /// How long each capture is shown right after it is taken.
    #[serde(default = "Countdown::default_capture_review")]
    pub capture_review_seconds: f32,
    /// How long the choose screen waits, since the last tap, before saving the
    /// photos as they are.
    #[serde(default = "Countdown::default_retake_choice")]
    pub retake_choice_seconds: f32,
    /// How long the finished sheet stays on screen before returning to idle.
    #[serde(default = "Countdown::default_review")]
    pub review_seconds: f32,
}

impl Countdown {
    fn default_seconds() -> u32 {
        3
    }
    fn default_between() -> f32 {
        1.5
    }
    fn default_capture_review() -> f32 {
        1.0
    }
    fn default_retake_choice() -> f32 {
        30.0
    }
    fn default_review() -> f32 {
        15.0
    }
}

impl Default for Countdown {
    fn default() -> Self {
        Self {
            seconds: Self::default_seconds(),
            between_captures_seconds: Self::default_between(),
            capture_review_seconds: Self::default_capture_review(),
            retake_choice_seconds: Self::default_retake_choice(),
            review_seconds: Self::default_review(),
        }
    }
}

/// One selectable photobooth mode: how many shots and how they are laid out.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Number of captures taken in this mode.
    pub captures: u32,
    /// Columns in the capture grid; rows follow from `captures`.
    #[serde(default = "Mode::default_columns")]
    pub columns: u32,
    /// Final sheet size in pixels.
    pub sheet: [u32; 2],
    /// Border between sheet edge and the capture grid, in pixels.
    #[serde(default = "Mode::default_margin")]
    pub margin: u32,
    /// Gap between capture cells, in pixels.
    #[serde(default = "Mode::default_gap")]
    pub gap: u32,
    /// Sheet background, `#rrggbb`.
    #[serde(default = "Mode::default_background")]
    pub background: String,
    /// Reserved strip at the bottom of the sheet for text, in pixels.
    #[serde(default)]
    pub footer_height: u32,
    #[serde(default)]
    pub footer_text: String,
    #[serde(default = "Mode::default_footer_color")]
    pub footer_color: String,
    /// PNG composited over the finished sheet (watermark / frame). Scaled to
    /// the sheet size.
    #[serde(default)]
    pub overlay: Option<PathBuf>,
}

impl Mode {
    fn default_columns() -> u32 {
        1
    }
    fn default_margin() -> u32 {
        40
    }
    fn default_gap() -> u32 {
        24
    }
    fn default_background() -> String {
        "#ffffff".to_string()
    }
    fn default_footer_color() -> String {
        "#111111".to_string()
    }

    /// Rows needed to hold `captures` at `columns` wide.
    pub fn rows(&self) -> u32 {
        self.captures.div_ceil(self.columns.max(1))
    }

    /// `[width, height]` of one capture cell once margins, gaps and the footer
    /// band are taken out of the sheet.
    pub fn cell_size(&self) -> Result<[u32; 2]> {
        let columns = self.columns.max(1);
        let rows = self.rows();
        let margins = self.margin.saturating_mul(2);
        let width = cell_length(self.sheet[0], margins, self.gap, columns);
        let height = cell_length(
            self.sheet[1],
            margins.saturating_add(self.footer_height),
            self.gap,
            rows,
        );
        match (width, height) {
            (Some(width), Some(height)) => Ok([width, height]),
            _ => bail!(
                "mode '{}': margins, gaps and footer leave no room for {columns}x{rows} cells",
                self.id
            ),
        }
    }
}

/// Length of each of `cells` cells laid along `extent`, after `reserved`
/// pixels and the gaps between cells. `None` when nothing is left.
fn cell_length(extent: u32, reserved: u32, gap: u32, cells: u32) -> Option<u32> {
    let gaps = gap.saturating_mul(cells.saturating_sub(1));
    let free = extent.checked_sub(reserved)?.checked_sub(gaps)?;
    free.checked_div(cells).filter(|&length| length > 0)
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path, home: Option<&Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.general.output_dir = expand_tilde(&config.general.output_dir, home);
        config.general.font = config
            .general
            .font
            .as_deref()
            .map(|font| expand_tilde(font, home));
        config.general.log_file = config
            .general
            .log_file
            .as_deref()
            .map(|log_file| expand_tilde(log_file, home));
        for mode in &mut config.modes {
            mode.overlay = mode
                .overlay
                .as_deref()
                .map(|overlay| expand_tilde(overlay, home));
        }
        config.validate()?;
        Ok(config)
    }

    /// Everything that would otherwise only fail once a session is under way -
    /// after the guests have posed - is checked here, at startup.
    fn validate(&self) -> Result<()> {
        if self.modes.is_empty() {
            bail!("config defines no [[modes]] - at least one is required");
        }
        let mut ids = HashSet::new();
        for mode in &self.modes {
            if !ids.insert(mode.id.as_str()) {
                bail!("mode id '{}' is used more than once", mode.id);
            }
            if mode.captures == 0 {
                bail!("mode '{}' has captures = 0", mode.id);
            }
            if mode.columns == 0 {
                bail!("mode '{}' has columns = 0", mode.id);
            }
            if mode.sheet[0] == 0 || mode.sheet[1] == 0 {
                bail!("mode '{}' has a zero-sized sheet", mode.id);
            }
            if mode.footer_height >= mode.sheet[1] {
                bail!("mode '{}' footer_height exceeds the sheet height", mode.id);
            }
            parse_hex_color(&mode.background)
                .with_context(|| format!("mode '{}' background", mode.id))?;
            parse_hex_color(&mode.footer_color)
                .with_context(|| format!("mode '{}' footer_color", mode.id))?;
            mode.cell_size()?;
            if let Some(overlay) = &mode.overlay
                && !overlay.is_file()
            {
                bail!(
                    "mode '{}' overlay {} does not exist",
                    mode.id,
                    overlay.display()
                );
            }
        }
        for (key, seconds) in [
            (
                "between_captures_seconds",
                self.countdown.between_captures_seconds,
            ),
            (
                "capture_review_seconds",
                self.countdown.capture_review_seconds,
            ),
            (
                "retake_choice_seconds",
                self.countdown.retake_choice_seconds,
            ),
            ("review_seconds", self.countdown.review_seconds),
        ] {
            if Duration::try_from_secs_f32(seconds).is_err() {
                bail!("countdown.{key} must be a non-negative number of seconds (got {seconds})");
            }
        }
        if !matches!(self.camera.rotation, 0 | 90 | 180 | 270) {
            bail!(
                "camera.rotation must be 0, 90, 180 or 270 (got {})",
                self.camera.rotation
            );
        }
        Ok(())
    }
}

/// Config file search order: explicit path, then `./rsbooth.toml`, then
/// `<user config dir>/rsbooth/rsbooth.toml`.
pub fn resolve_config_path(explicit: Option<PathBuf>, user_dirs: &UserDirs) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = expand_tilde(&path, user_dirs.home.as_deref());
        if !path.exists() {
            bail!("config file {} does not exist", path.display());
        }
        return Ok(path);
    }
    let local = PathBuf::from(CONFIG_FILE_NAME);
    if local.exists() {
        return Ok(local);
    }
    let user = user_config_path(user_dirs)?;
    if user.exists() {
        return Ok(user);
    }
    bail!(
        "no config found: pass --config, or create ./{CONFIG_FILE_NAME}, \
         or run `rsbooth init-config` to write {}",
        user.display()
    )
}

pub fn user_config_path(user_dirs: &UserDirs) -> Result<PathBuf> {
    let dir = user_dirs
        .config
        .as_ref()
        .ok_or_else(|| anyhow!("no user config directory on this platform"))?;
    Ok(dir.join(APP_DIR_NAME).join(CONFIG_FILE_NAME))
}

/// `~` and `~/...` expansion (`~user` is not supported). Left as-is when the
/// home directory is unknown.
pub fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let (Some(text), Some(home)) = (path.to_str(), home) else {
        return path.to_path_buf();
    };
    if text == "~" {
        return home.to_path_buf();
    }
    match text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        Some(rest) => home.join(rest.trim_start_matches(['/', '\\'])),
        None => path.to_path_buf(),
    }
}

/// `#rrggbb` / `rrggbb` -> RGB triple.
pub fn parse_hex_color(text: &str) -> Result<[u8; 3]> {
    let text = text.trim();
    let hex = text.strip_prefix('#').unwrap_or(text);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("expected a #rrggbb color, got {text:?}");
    }
    let value = u32::from_str_radix(hex, 16).with_context(|| format!("invalid color {text:?}"))?;
    Ok([
        (value >> 16) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors_round_trip() {
        assert_eq!(parse_hex_color("#ff8000").unwrap(), [255, 128, 0]);
        assert_eq!(parse_hex_color("000000").unwrap(), [0, 0, 0]);
        assert!(parse_hex_color("#fff").is_err());
        assert!(parse_hex_color("#gggggg").is_err());
        assert!(parse_hex_color("#+12345").is_err());
        assert!(parse_hex_color("##12345").is_err());
    }

    #[test]
    fn tilde_expands_only_for_the_current_user() {
        let home = Some(Path::new("/home/booth"));
        assert_eq!(
            expand_tilde(Path::new("~"), home),
            PathBuf::from("/home/booth")
        );
        assert_eq!(
            expand_tilde(Path::new("~/Pictures"), home),
            PathBuf::from("/home/booth/Pictures")
        );
        assert_eq!(
            expand_tilde(Path::new("~bob/x"), home),
            PathBuf::from("~bob/x")
        );
        assert_eq!(
            expand_tilde(Path::new("/srv/x"), home),
            PathBuf::from("/srv/x")
        );
        assert_eq!(expand_tilde(Path::new("~/x"), None), PathBuf::from("~/x"));
    }

    #[test]
    fn default_config_parses_and_validates() {
        let config = default_config();
        config.validate().expect("default config validates");
        assert!(!config.modes.is_empty());
    }

    fn test_mode() -> Mode {
        Mode {
            id: "m".into(),
            name: "m".into(),
            description: String::new(),
            captures: 3,
            columns: 2,
            sheet: [100, 100],
            margin: 0,
            gap: 0,
            background: "#ffffff".into(),
            footer_height: 0,
            footer_text: String::new(),
            footer_color: "#000000".into(),
            overlay: None,
        }
    }

    fn default_config() -> Config {
        toml::from_str(DEFAULT_CONFIG_TOML).expect("default config parses")
    }

    #[test]
    fn rows_round_up() {
        let mut mode = test_mode();
        assert_eq!(mode.rows(), 2);
        mode.captures = 4;
        assert_eq!(mode.rows(), 2);
        mode.columns = 1;
        assert_eq!(mode.rows(), 4);
    }

    #[test]
    fn cells_fill_what_margins_gaps_and_footer_leave() {
        let mut mode = test_mode();
        mode.sheet = [600, 900];
        mode.margin = 20;
        mode.gap = 10;
        mode.footer_height = 100;
        // (600 - 40 - 10) / 2 wide, (900 - 40 - 100 - 10) / 2 high.
        assert_eq!(mode.cell_size().unwrap(), [275, 375]);
        mode.margin = 400;
        assert!(mode.cell_size().is_err());
        mode.margin = u32::MAX;
        assert!(mode.cell_size().is_err());
    }

    #[test]
    fn layouts_that_cannot_fit_fail_validation() {
        let mut config = default_config();
        config.modes[0].margin = config.modes[0].sheet[0];
        assert!(config.validate().is_err());
    }

    #[test]
    fn duplicate_mode_ids_fail_validation() {
        let mut config = default_config();
        let duplicate = config.modes[0].clone();
        config.modes.push(duplicate);
        assert!(config.validate().is_err());
    }

    #[test]
    fn unusable_durations_fail_validation() {
        for seconds in [-1.0, f32::NAN, f32::INFINITY] {
            let mut config = default_config();
            config.countdown.review_seconds = seconds;
            assert!(config.validate().is_err(), "{seconds} accepted");
        }
    }

    #[test]
    fn missing_overlays_fail_validation() {
        let mut config = default_config();
        config.modes[0].overlay = Some(PathBuf::from("/nonexistent/rsbooth-overlay.png"));
        assert!(config.validate().is_err());
    }
}
