# rsbooth

A configurable photobooth for desktops, in Rust. Live webcam preview, a
countdown-triggered high-quality still, and layouts defined in a config file.
Photos are written to a local folder.

Inspired by [pibooth](https://github.com/pibooth/pibooth), but a native binary
with an egui touchscreen UI instead of a Python/pygame stack.

## What it does

1. **Idle** - full-screen live preview with a big touch target.
2. **Mode select** - one button per `[[modes]]` entry in the config (skipped
   when only one mode is configured). Returns to idle after 60 seconds with
   no choice.
3. **Countdown** - a per-shot countdown, then the shutter.
4. **Capture** - the still is grabbed from the already-running
   capture-resolution stream (see [Shutter timing](#shutter-timing)).
5. **Compose** - captures are centre-cropped into the mode's grid, drawn onto
   the sheet with margins, gaps, an optional footer and an optional PNG overlay.
6. **Save** - `<output_dir>/<timestamp>_<mode-id>/capture_N.jpg` plus
   `sheet.jpg`.

## Shutter timing

The photo that gets stored is the frame that was on screen when the picture
froze. This is the one thing a photobooth must not get wrong, and it is easy to
get wrong: switching a webcam to its full-resolution format takes anywhere from
tens of milliseconds to a couple of seconds, and no preview frames arrive while
it happens. Do that switch when the countdown hits zero and the screen freezes
on a *stale preview frame*, everyone relaxes, and the real shutter fires a
second or two later on a different pose.

So the switch is moved off the shutter path. The camera is **armed** when the
session starts - on the mode-select screen, or at the top of the countdown -
and the preview keeps running from the capture-resolution stream while the
countdown ticks. At zero, taking the photo is just "drop the queued frame, keep
the next one". The picture stays live right up to that moment, and the frame it
freezes on is the frame on disk.

Measure it on your own hardware:

```bash
rsbooth capture --mode strip4            # armed, the normal path
rsbooth capture --mode strip4 --no-arm   # diagnostic: switch at the shutter
```

Each shot prints its shutter lag - the gap between the countdown reaching zero
and the moment the stored frame was captured, taken from the driver's frame
timestamp where the backend provides one. A negative lag means the frame was
captured *before* zero. Against the mock camera, which models a 1.2s
reconfiguration, that is 15-65ms armed versus ~1250ms unarmed. The booth also
logs a warning on screen if a real capture is ever more than 250ms either side
of zero.

The camera thread only pulls frames off the driver; a separate thread decodes
the newest one for the preview and skips any it cannot keep up with. On a slow
CPU (a Pi 3 decoding 1080p) the preview frame rate drops while armed, but the
driver's queue never fills with old frames and the shutter is answered within
a frame.

Two config keys tune this:

- `camera.warmup_frames` - frames burned right after arming, so exposure and
  white balance settle during the countdown instead of at the shutter.
- `camera.capture_discard_frames` - frames dropped at the shutter before the
  kept one, guaranteeing the stored frame was exposed *after* zero rather than
  pulled from the driver's queue.

Keep `countdown.seconds` at 2 or more when `capture_resolution` differs from
`preview_resolution`, so arming has time to finish.

## Build and run

```bash
cargo build --release
cargo run --release -- run
```

Linux needs V4L2 headers at build time (`libv4l-dev` on Debian/Ubuntu) and a
GPU or software GL stack at run time.

### Build features

| Feature | Default | Effect |
| --- | --- | --- |
| `wgpu` | yes | wgpu renderer (Vulkan / Metal / DX12 / GL) |
| `glow` | no | OpenGL ES renderer, via glutin |
| `fast-jpeg` | no | MJPEG decoding through mozjpeg's SIMD decoder instead of the pure-Rust one |
| `pi` | no | `glow` + `fast-jpeg` |

`fast-jpeg` needs **NASM** when building for x86 (`apt install nasm`). ARM
targets - the Pi, and Windows-on-ARM under WSL - use GAS assembly instead and
need no extra tool.

## Raspberry Pi

```bash
cargo build --release --no-default-features --features pi
```

Why those features:

- **`glow`.** The Pi's V3D driver is far better exercised through OpenGL ES
  than through Vulkan, which is what wgpu would reach for.
- **`fast-jpeg`.** Decoding one 720p MJPEG frame, measured on an aarch64
  desktop core: 7.5ms with the pure-Rust decoder, 2.5ms with mozjpeg's NEON
  one. A Pi core is several times slower again, which puts the pure-Rust path
  at or past the 33ms budget for a 30fps preview and mozjpeg comfortably
  inside it.

Composition is not a bottleneck: a four-shot 1200x3600 sheet takes ~150ms of
Lanczos3 resizing plus ~35ms of JPEG encoding on that same core, so roughly a
second on a Pi 4 - once per session, behind the "Making your photos" screen.

Suggested config on a Pi:

```toml
[window]
fullscreen = true
hide_cursor = true

[camera]
# USB webcams stream MJPEG; YUYV at 720p will not fit in USB 2.0 bandwidth.
format = "mjpeg"
preview_resolution = [1280, 720]
capture_resolution = [1920, 1080]
```

**Camera support:** rsbooth talks V4L2, which covers USB webcams. The CSI
camera modules on current Pi OS (Bookworm and later) go through libcamera and
are *not* reachable this way - `rsbooth list-cameras` will not show them.

**Starting at boot:** with desktop autologin on (`raspi-config` > System
Options > Boot / Auto Login), add `~/.config/autostart/rsbooth.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=rsbooth
Exec=/home/pi/rsbooth --config /home/pi/rsbooth.toml run
```

Autostart does not run from your home directory, hence the explicit
`--config`. Also turn screen blanking off (`raspi-config` > Display Options >
Screen Blanking), or the idle screen goes dark. **Esc** on the idle screen quits
back to the desktop.

## Usage

```bash
rsbooth init-config          # write ~/.config/rsbooth/rsbooth.toml
rsbooth list-cameras         # find the camera index
rsbooth run                  # the booth itself (default command)
rsbooth capture --mode strip4  # one session, headless: shoot, compose, save
```

`--config <path>` overrides the config search order, which is `./rsbooth.toml`,
then `~/.config/rsbooth/rsbooth.toml`.

Keyboard: **Space/Enter** starts a session, **Esc** cancels one. **Esc** on the
idle screen quits back to the desktop.

`RSBOOTH_LOG` sets the log filter (default `rsbooth=info,warn`); an invalid
filter is an error.

## Configuration

Everything lives in `rsbooth.toml`; the bundled file documents each key.
The whole file is validated at startup - including whether every layout
leaves room for its cells and every overlay exists - so a bad config fails
before anyone poses, not after.

- `[general]` - `output_dir`, `idle_text`, and `font` (a TTF/OTF path used for
  footer text; footer text is skipped with a warning when it is unset).
- `[window]` - `fullscreen`, `hide_cursor`, `size`, `flash`.
- `[camera]` - `backend` (`webcam` or `mock`), `index`, `preview_resolution`,
  `capture_resolution`, `format`, `warmup_frames`, `capture_discard_frames`,
  `rotation`, `mirror` (preview and saved photos alike).
- `[countdown]` - `seconds`, `between_captures_seconds`,
  `capture_review_seconds`, `review_seconds`.
- `[[modes]]` - one per selectable layout: `captures`, `columns`, `sheet`
  size, `margin`, `gap`, `background`, `footer_height`, `footer_text`,
  `footer_color`, `overlay`.

### Adding a layout

```toml
[[modes]]
id = "grid6"
name = "Six Up"
description = "6 shots, 2 columns"
captures = 6
columns = 2
sheet = [2400, 3600]
margin = 60
gap = 30
background = "#ffffff"
footer_height = 300
footer_text = "Our Wedding"
footer_color = "#111111"
```

Cells are sized from the sheet, margins and gaps; captures are centre-cropped
to the cell aspect ratio, never letterboxed.

### No camera attached?

Set `backend = "mock"` for synthetic frames. The whole pipeline - countdown,
capture, compose, save - runs unchanged, which is how the layout tests and
`rsbooth capture` work on machines with no `/dev/video*`. The mock deliberately
models a real camera's 1.2s stream reconfiguration, so the arming path above is
exercised rather than assumed.

## Layout of the code

| File | Responsibility |
| --- | --- |
| `src/main.rs` | CLI, environment variables (read once), window setup |
| `src/config.rs` | TOML schema, defaults, validation |
| `src/camera.rs` | Capture thread, preview decoder thread, format switching, pixel-format decoding |
| `src/layout.rs` | Sheet composition, footer text, JPEG output |
| `src/session.rs` | Session naming, background compose/save worker |
| `src/ui.rs` | egui state machine and drawing |

`cargo test` covers pixel-format decoding, layout geometry, config validation
and session output.
