//! A booth session: the captures taken for one mode, and the work of turning
//! them into files on disk.
//!
//! Composing and JPEG-encoding a multi-megapixel sheet takes long enough to
//! drop frames, so it runs on a worker thread and reports back over a channel.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use ab_glyph::FontVec;
use anyhow::{Context, Result};
use chrono::Local;
use image::RgbImage;

use crate::config::Mode;
use crate::layout;

/// Timestamp format used for session directory names. Sorts chronologically.
const SESSION_STAMP_FORMAT: &str = "%Y%m%d-%H%M%S";

/// Longest edge of the sheet thumbnail handed to the UI for the review screen.
const REVIEW_PREVIEW_EDGE: u32 = 1400;

/// One finished session, written to disk.
pub struct Processed {
    /// Directory holding the captures and the sheet.
    pub directory: PathBuf,
    pub sheet_path: PathBuf,
    /// The individual captures, in shooting order.
    pub capture_paths: Vec<PathBuf>,
    /// Downscaled sheet for on-screen review.
    pub preview: RgbImage,
}

/// Inputs for the processing thread.
pub struct ProcessRequest {
    pub mode: Mode,
    pub captures: Vec<RgbImage>,
    pub output_dir: PathBuf,
    pub font: Option<Arc<FontVec>>,
}

/// Compose and save on a worker thread.
pub fn spawn_processing(request: ProcessRequest) -> Receiver<Result<Processed>> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("rsbooth-processing".to_string())
        .spawn(move || {
            let _ = tx.send(process(request));
        })
        .expect("spawning the processing thread");
    rx
}

fn process(request: ProcessRequest) -> Result<Processed> {
    let name = session_name(&request.mode.id);
    let directory = request.output_dir.join(&name);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;

    let mut capture_paths = Vec::with_capacity(request.captures.len());
    for (index, capture) in request.captures.iter().enumerate() {
        let path = directory.join(format!("capture_{}.jpg", index + 1));
        layout::save_jpeg(capture, &path)?;
        capture_paths.push(path);
    }

    let sheet = layout::compose(&request.mode, &request.captures, request.font.as_deref())?;
    let sheet_path = directory.join("sheet.jpg");
    layout::save_jpeg(&sheet, &sheet_path)?;

    let preview = layout::thumbnail(sheet, REVIEW_PREVIEW_EDGE);
    Ok(Processed {
        directory,
        sheet_path,
        capture_paths,
        preview,
    })
}

/// `20260830-134501_strip4`
fn session_name(mode_id: &str) -> String {
    format!(
        "{}_{}",
        Local::now().format(SESSION_STAMP_FORMAT),
        sanitize(mode_id)
    )
}

/// Keep mode ids safe to use in a directory name on any file system.
fn sanitize(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "mode".to_string()
    } else {
        cleaned
    }
}

/// Load the footer font, warning instead of failing when it is unusable - a
/// bad font path should not stop the booth from taking photos.
pub fn load_font_or_warn(path: Option<&Path>) -> Option<Arc<FontVec>> {
    let path = path?;
    match layout::load_font(path) {
        Ok(font) => Some(Arc::new(font)),
        Err(error) => {
            tracing::warn!("footer text disabled: {error:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn test_mode() -> Mode {
        Mode {
            id: "strip4".into(),
            name: "Strip".into(),
            description: String::new(),
            captures: 2,
            columns: 1,
            sheet: [400, 800],
            margin: 10,
            gap: 10,
            background: "#ffffff".into(),
            footer_height: 0,
            footer_text: String::new(),
            footer_color: "#000000".into(),
            overlay: None,
        }
    }

    #[test]
    fn session_names_are_sortable_and_safe() {
        let name = session_name("strip/4 fancy");
        assert!(name.ends_with("_strip-4-fancy"), "got {name}");
        assert_eq!(name.len(), "20260830-134501".len() + "_strip-4-fancy".len());
    }

    #[test]
    fn processing_writes_captures_and_a_sheet() {
        let temp = std::env::temp_dir().join(format!(
            "rsbooth-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let processed = process(ProcessRequest {
            mode: test_mode(),
            captures: vec![RgbImage::from_pixel(64, 48, Rgb([1, 2, 3])); 2],
            output_dir: temp.clone(),
            font: None,
        })
        .unwrap();

        assert!(processed.sheet_path.exists());
        assert_eq!(processed.capture_paths.len(), 2);
        assert!(processed.capture_paths.iter().all(|path| path.exists()));
        assert!(processed.directory.join("capture_1.jpg").exists());
        assert!(processed.directory.join("capture_2.jpg").exists());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn review_preview_is_capped() {
        let preview = layout::thumbnail(RgbImage::new(4000, 2000), REVIEW_PREVIEW_EDGE);
        assert_eq!(preview.width(), REVIEW_PREVIEW_EDGE);
        assert_eq!(preview.height(), REVIEW_PREVIEW_EDGE / 2);
    }
}
