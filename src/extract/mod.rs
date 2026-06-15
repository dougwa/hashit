//! The extraction engine: identify a file, pull metadata, make a thumbnail.
//!
//! Extraction runs **once per content hash** (see `index.rs`), so the cost of
//! reading a file is amortized across all its duplicate copies. The default
//! implementations are in-process (`exiftool_rs`, `magick-*`); the split into
//! free functions here leaves room for a trait-based external-tool fallback
//! later without changing callers.

mod exif;
mod thumb;

use std::path::Path;

use anyhow::Result;

use crate::store::ContentMeta;

/// Bump when extraction logic changes meaningfully, so a future pass can decide
/// to re-extract content whose stored `extractor_version` is older.
pub const EXTRACTOR_VERSION: i64 = 1;

/// How many leading bytes to read for magic-number file-type detection.
const HEADER_BYTES: usize = 64 * 1024;

/// Identify a file's coarse type category ("image", "video", …) and extension.
/// Reads only a header, so large non-media files aren't fully read here.
fn identify(path: &Path) -> (Option<String>, Option<String>) {
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

fn read_header(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; HEADER_BYTES];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Extract everything we can for one file: type/ext, metadata tags, and (for
/// images) a thumbnail written to `thumb_dest`. Never fails the whole index run
/// for a single unreadable/odd file — metadata or thumbnail failures degrade to
/// empty/absent, surfaced via the returned `ContentMeta`.
pub fn extract_all(path: &Path, size: u64, thumb_dest: &Path) -> ContentMeta {
    let (file_type, ext) = identify(path);
    let is_image = file_type.as_deref() == Some("image");

    let tags = if is_image {
        exif::extract_tags(path).unwrap_or_default()
    } else {
        Vec::new()
    };

    let has_thumb = if is_image {
        match thumb::generate(path, thumb_dest) {
            Ok(()) => true,
            Err(_) => false,
        }
    } else {
        false
    };

    ContentMeta {
        size,
        file_type,
        ext,
        extractor_version: EXTRACTOR_VERSION,
        has_thumb,
        tags,
    }
}

/// Generate a thumbnail for an already-known content hash on demand (used by
/// `hashit thumb` when the cache is missing one). Returns whether one was made.
pub fn make_thumb(src: &Path, dest: &Path) -> bool {
    thumb::generate(src, dest).is_ok()
}
