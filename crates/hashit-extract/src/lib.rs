//! The extraction engine: identify a file, pull metadata, make a thumbnail.
//!
//! Extraction runs **once per content hash** (the caller dedups by hash), so the
//! cost of reading a file is amortized across all its duplicate copies. The
//! default implementations are in-process (`exiftool_rs`, `magick-*`); the split
//! into free functions here leaves room for a trait-based external-tool fallback
//! later without changing callers.

mod exif;
mod preview;
mod thumb;

use std::path::Path;

use serde::Serialize;

/// Bump when extraction logic changes meaningfully, so a future pass can decide
/// to re-extract content whose stored `extractor_version` is older.
pub const EXTRACTOR_VERSION: i64 = 1;

/// How many leading bytes to read for magic-number file-type detection.
const HEADER_BYTES: usize = 64 * 1024;

/// One flattened metadata tag: an EXIF group, key, and rendered value.
#[derive(Debug, Clone, Serialize)]
pub struct MetaTag {
    pub group: String,
    pub key: String,
    pub value: String,
}

/// Everything extracted from a single file (keyed by content hash upstream).
#[derive(Debug, Clone, Serialize)]
pub struct Extracted {
    pub file_type: Option<String>,
    pub ext: Option<String>,
    pub extractor_version: i64,
    pub tags: Vec<MetaTag>,
}

/// Identify a file's coarse type category ("image", "video", …) and extension.
/// Reads only a header, so large non-media files aren't fully read here.
pub fn identify(path: &Path) -> (Option<String>, Option<String>) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    let header = read_header(path).unwrap_or_default();
    let ft = exiftool_rs::filetype::identify(&header, ext.as_deref());
    let category = ft.map(|f| {
        // "image/jpeg" -> "image"; fall back to the whole mime if unsplit.
        f.mime.split('/').next().unwrap_or(f.mime).to_string()
    });
    (category, ext)
}

fn read_header(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; HEADER_BYTES];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Identify a file and (for images) pull its EXIF tags. Never fails on an odd or
/// unreadable file — metadata failures degrade to an empty tag list.
pub fn extract_tags(path: &Path) -> Extracted {
    let (file_type, ext) = identify(path);
    let is_image = file_type.as_deref() == Some("image");
    let tags = if is_image {
        exif::extract_tags(path).unwrap_or_default()
    } else {
        Vec::new()
    };
    Extracted {
        file_type,
        ext,
        extractor_version: EXTRACTOR_VERSION,
        tags,
    }
}

/// Generate a downscaled JPEG thumbnail for `src` at `dest`. Returns whether one
/// was produced (false for non-images / undecodable formats).
pub fn make_thumbnail(src: &Path, dest: &Path) -> bool {
    thumb::generate(src, dest).is_ok()
}

/// Generate a larger JPEG preview for `src` at `dest`. Returns whether one was
/// produced (false for non-images / undecodable formats).
pub fn make_preview(src: &Path, dest: &Path) -> bool {
    preview::generate(src, dest).is_ok()
}
