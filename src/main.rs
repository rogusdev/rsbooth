//! rsbooth - a configurable photobooth for desktops.
//!
//! Live preview from a webcam, a countdown-triggered high-quality still, and a
//! composed photo sheet per the selected mode, all written to a local folder.

mod camera;
mod config;
mod layout;
mod session;
mod ui;

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::camera::{CameraEvent, CameraHandle, Capture};
use crate::config::{Config, UserDirs};
use crate::session::ProcessRequest;

/// Overrides the tracing filter (e.g. `RSBOOTH_LOG=debug`).
const LOG_ENV_VAR: &str = "RSBOOTH_LOG";

/// Default tracing filter when `RSBOOTH_LOG` is unset.
const DEFAULT_LOG_FILTER: &str = "rsbooth=info,warn";

/// Window title, also used as the Wayland/X11 app id.
const APP_TITLE: &str = "rsbooth";

/// Renderer chosen at build time. The Raspberry Pi's V3D driver is far happier
/// with OpenGL ES (glow) than with Vulkan through wgpu, so `--features pi`
/// switches this over.
#[cfg(not(any(feature = "wgpu", feature = "glow")))]
compile_error!("enable a renderer: the `wgpu` feature (default) or `glow`");
#[cfg(feature = "glow")]
const RENDERER: eframe::Renderer = eframe::Renderer::Glow;
#[cfg(not(feature = "glow"))]
const RENDERER: eframe::Renderer = eframe::Renderer::Wgpu;

/// How long `rsbooth capture` waits for the camera to report a still.
const HEADLESS_CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Polling interval while waiting on the camera.
const HEADLESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Parser)]
#[command(name = "rsbooth", about = "A configurable photobooth", version)]
struct Cli {
    /// Config file to use. Defaults to ./rsbooth.toml, then the user config
    /// directory.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the booth (default).
    Run,
    /// Run one session headlessly: capture, compose and save without opening a
    /// window. Useful for testing a config or measuring shutter lag.
    Capture {
        /// Mode id to shoot. Defaults to the first configured mode.
        #[arg(long)]
        mode: Option<String>,
        /// Skip arming, so the stream reconfiguration lands on the shutter.
        /// Diagnostic only: measures what the lag would be without the fix.
        #[arg(long)]
        no_arm: bool,
    },
    /// List cameras visible to the platform backend.
    ListCameras,
    /// Write the bundled config to the user config directory.
    InitConfig {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    // Every environment variable the program reads is read here, once. The
    // `dirs` lookups read `$HOME` and `$XDG_CONFIG_HOME`.
    let log_filter = std::env::var(LOG_ENV_VAR).unwrap_or_else(|_| DEFAULT_LOG_FILTER.to_string());
    let user_dirs = UserDirs {
        home: dirs::home_dir(),
        config: dirs::config_dir(),
    };

    let log_filter = EnvFilter::try_new(&log_filter)
        .with_context(|| format!("invalid {LOG_ENV_VAR} filter {log_filter:?}"))?;

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            let (path, config) = load_config(cli.config, &user_dirs)?;
            init_logging(log_filter, Some(&config.general.log_path()))?;
            tracing::info!("loaded {}", path.display());
            run_booth(config)
        }
        Command::Capture { mode, no_arm } => {
            let (path, config) = load_config(cli.config, &user_dirs)?;
            init_logging(log_filter, Some(&config.general.log_path()))?;
            tracing::info!("loaded {}", path.display());
            capture_headless(&path, config, mode.as_deref(), !no_arm)
        }
        Command::ListCameras => {
            init_logging(log_filter, None)?;
            list_cameras()
        }
        Command::InitConfig { force } => {
            init_logging(log_filter, None)?;
            init_config(&user_dirs, force)
        }
    }
}

fn load_config(config_path: Option<PathBuf>, user_dirs: &UserDirs) -> Result<(PathBuf, Config)> {
    let path = config::resolve_config_path(config_path, user_dirs)?;
    let config = Config::load(&path, user_dirs.home.as_deref())?;
    Ok((path, config))
}

/// Log to stdout, and also append to `log_file` when given.
fn init_logging(filter: EnvFilter, log_file: Option<&Path>) -> Result<()> {
    let file_layer = log_file
        .map(|path| -> Result<_> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening log file {}", path.display()))?;
            Ok(tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(file)))
        })
        .transpose()?;
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(file_layer)
        .init();
    Ok(())
}

fn run_booth(config: Config) -> Result<()> {
    let viewport = egui_viewport(&config);
    let options = eframe::NativeOptions {
        viewport,
        renderer: RENDERER,
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(move |cc| {
            apply_touch_style(&cc.egui_ctx);
            Ok(Box::new(ui::BoothApp::new(config)?))
        }),
    )
    .map_err(|error| anyhow::anyhow!("running the window: {error}"))
}

fn egui_viewport(config: &Config) -> eframe::egui::ViewportBuilder {
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title(APP_TITLE)
        .with_app_id(APP_TITLE)
        .with_inner_size(config.window.size);
    if config.window.fullscreen {
        viewport = viewport.with_fullscreen(true);
    }
    viewport
}

/// Bigger hit targets than egui's defaults - every control is meant to be
/// pressed with a finger on a touchscreen.
fn apply_touch_style(ctx: &eframe::egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.button_padding = eframe::egui::vec2(24.0, 16.0);
        style.spacing.item_spacing = eframe::egui::vec2(12.0, 12.0);
        style.visuals.widgets.inactive.corner_radius = 12.into();
        style.visuals.widgets.hovered.corner_radius = 12.into();
        style.visuals.widgets.active.corner_radius = 12.into();
    });
}

fn list_cameras() -> Result<()> {
    let cameras = camera::list_cameras()?;
    if cameras.is_empty() {
        println!("No cameras found.");
        return Ok(());
    }
    for (index, name) in cameras {
        println!("{index}\t{name}");
    }
    Ok(())
}

fn init_config(user_dirs: &UserDirs, force: bool) -> Result<()> {
    let path = config::user_config_path(user_dirs)?;
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, config::DEFAULT_CONFIG_TOML)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("Wrote {}", path.display());
    Ok(())
}

/// One full session with no UI: shoot, compose, save, then report.
///
/// Mirrors the UI's ordering - arm, wait out the countdown, shoot - so this is
/// also how the shutter lag is measured on real hardware.
fn capture_headless(
    config_path: &Path,
    config: Config,
    mode_id: Option<&str>,
    arm: bool,
) -> Result<()> {
    let mode = match mode_id {
        Some(id) => config
            .modes
            .iter()
            .find(|mode| mode.id == id)
            .with_context(|| format!("no mode with id {id:?} in {}", config_path.display()))?,
        None => &config.modes[0],
    }
    .clone();

    let camera = CameraHandle::spawn(config.camera.clone());
    let countdown = Duration::from_secs(u64::from(config.countdown.seconds));
    let mut captures = Vec::new();
    for shot in 1..=mode.captures {
        if arm {
            camera.arm();
        }
        let shutter_at = Instant::now() + countdown;
        println!(
            "shot {shot}/{}: {countdown:?} countdown{}",
            mode.captures,
            if arm { "" } else { " (unarmed)" }
        );
        while Instant::now() < shutter_at {
            // No capture has been requested yet, so none can arrive.
            report_camera_events(&camera)?;
            std::thread::sleep(HEADLESS_POLL_INTERVAL);
        }
        camera.request_capture();
        let capture = wait_for_capture(&camera)?;
        println!("  shutter lag: {:+.0}ms", capture.offset_ms(shutter_at));
        captures.push(capture.image);
    }
    camera.disarm();
    drop(camera);

    let processed = session::spawn_processing(ProcessRequest {
        mode,
        captures,
        output_dir: config.general.output_dir.clone(),
        font: session::load_font_or_warn(config.general.font.as_deref()),
    })
    .recv()
    .context("the processing thread died")??;

    for path in processed
        .capture_paths
        .iter()
        .chain([&processed.sheet_path])
    {
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Drain and print camera events, so warnings are not swallowed. Returns the
/// still if one arrived.
fn report_camera_events(camera: &CameraHandle) -> Result<Option<Capture>> {
    let mut captured = None;
    for event in camera.poll_events() {
        match event {
            CameraEvent::Ready(description) => println!("camera: {description}"),
            CameraEvent::Armed { description, took } => {
                println!("  armed at {description} in {took:?}");
            }
            CameraEvent::Warning(warning) => eprintln!("  warning: {warning}"),
            CameraEvent::CaptureFailed(error) => bail!("capture failed: {error}"),
            CameraEvent::Fatal(error) => bail!("camera stopped: {error}"),
            CameraEvent::Captured(capture) => captured = Some(*capture),
        }
    }
    Ok(captured)
}

fn wait_for_capture(camera: &CameraHandle) -> Result<Capture> {
    let deadline = Instant::now() + HEADLESS_CAPTURE_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(capture) = report_camera_events(camera)? {
            return Ok(capture);
        }
        std::thread::sleep(HEADLESS_POLL_INTERVAL);
    }
    bail!("timed out waiting for a capture")
}
