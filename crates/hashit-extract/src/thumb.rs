//! Thumbnail generation via the magick-rs crates.

use std::path::Path;

use anyhow::{Context, Result};
use magick_core::ops::resize::resize;
use magick_core::Geometry;

/// Shrink-only fit into a 512×512 box (the `>` flag leaves smaller images as-is).
const THUMB_GEOMETRY: &str = "512x512>";

/// Decode `src`, downscale it, and write a JPEG thumbnail to `dest` (creating
/// parent dirs). Errors (undecodable formats like some RAW/HEIC) propagate so
/// the caller can record "no thumbnail" without failing the whole index run.
pub fn generate(src: &Path, dest: &Path) -> Result<()> {
    let mut img =
        magick_codecs::read(src).with_context(|| format!("decoding {}", src.display()))?;
    let geom = Geometry::parse(THUMB_GEOMETRY)?;
    resize(&mut img, &geom);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    magick_codecs::write(&img, dest).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}
