//! Preview generation via the magick-rs crates.
//!
//! Same code path as `thumb`, but downscales into a larger box so the result is
//! suitable for a full-screen viewer rather than a grid cell.

use std::path::Path;

use anyhow::{Context, Result};
use magick_core::ops::resize::resize;
use magick_core::Geometry;

/// Shrink-only fit into a 2048×2048 box (the `>` flag leaves smaller images as-is).
const PREVIEW_GEOMETRY: &str = "2048x2048>";

/// Decode `src`, downscale it, and write a JPEG preview to `dest` (creating
/// parent dirs). Errors (undecodable formats like some RAW/HEIC) propagate so
/// the caller can record "no preview" without failing the whole run.
pub fn generate(src: &Path, dest: &Path) -> Result<()> {
    let mut img =
        magick_codecs::read(src).with_context(|| format!("decoding {}", src.display()))?;
    let geom = Geometry::parse(PREVIEW_GEOMETRY)?;
    resize(&mut img, &geom);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    magick_codecs::write(&img, dest).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}
