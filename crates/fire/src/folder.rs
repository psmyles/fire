//! Folder navigation: the sibling-image cursor behind ←/→ and the status-bar image count.
//!
//! Opening a file kicks off a background scan of its directory (off the UI thread, like a
//! decode) that posts the sorted list of sibling images back to the frame. Until that lands
//! the cursor is `None` — arrows do nothing and the count is hidden — so the image shows
//! first and the count fills in after (lazy, as requested). The cursor is a *snapshot* taken
//! at open time; it is not re-scanned as you page through, so adding/removing files in the
//! folder while it's open won't be reflected until the next fresh open.
//!
//! The scan and sort are pure (no Win32), so they run on the scan thread and are unit-tested
//! here; the win shell only owns the message plumbing.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::str::Chars;

/// Image file extensions fire can open. Kept in sync with the installer's per-format
/// associations (`installer/fire.iss` `[Tasks]`): if a format is associated there, a folder
/// of those files should navigate here. Lower-case; matching is case-insensitive.
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "tif", "tiff", "webp", "ico",
    "tga", "qoi", "ppm", "pgm", "pbm", "pnm", "ff", "jxl", "hdr", "exr", "psd", "psb", "heic",
    "heif", "avif",
    // Camera raw (embedded-preview decode; kept in sync with fire-decode's EXT_LABELS).
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef", "srw",
    "dng", "x3f", "3fr", "fff", "iiq", "erf", "mrw", "dcr", "kdc", "mef", "mos", "rwl", "gpr",
    "raw",
];

/// Whether `path`'s extension is one fire decodes (the folder-navigation membership test).
pub fn is_supported_image(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let ext = ext.to_ascii_lowercase();
            IMAGE_EXTS.contains(&ext.as_str())
        }
        None => false,
    }
}

/// Scan `file`'s directory for sibling image files, sorted in file-manager order. Returns the
/// full paths (including `file` itself if it is a supported image). Any I/O failure (no
/// parent, unreadable dir) yields an empty list — the caller then simply gets no cursor.
pub fn scan(file: &Path) -> Vec<PathBuf> {
    let Some(dir) = file.parent() else { return Vec::new() };
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };

    // Pair each path with its file name so the natural sort doesn't re-extract it per compare.
    let mut named: Vec<(String, PathBuf)> = read
        .filter_map(|e| e.ok())
        // file_type() comes free from the directory enumeration on Windows (no extra stat), so
        // excluding subdirectories named like images is cheap.
        .filter(|e| e.file_type().map(|t| !t.is_dir()).unwrap_or(true))
        .map(|e| e.path())
        .filter(|p| is_supported_image(p))
        .filter_map(|p| Some((p.file_name()?.to_string_lossy().into_owned(), p)))
        .collect();
    named.sort_by(|(a, _), (b, _)| natural_cmp(a, b));
    named.into_iter().map(|(_, p)| p).collect()
}

/// Sorted sibling image paths in a directory plus the index of the current one. Constructed
/// from a finished [`scan`]; positioned at the image that was open when the scan was issued.
pub struct Folder {
    entries: Vec<PathBuf>,
    current: usize,
}

impl Folder {
    /// Build a cursor from a scan's `entries`, positioned at `current` (the open image).
    /// Returns `None` if the open image isn't among the entries (e.g. it was deleted between
    /// the open and the scan, or opened with an unrecognized extension) — then there's nothing
    /// meaningful to page through, so the caller shows no cursor.
    pub fn new(entries: Vec<PathBuf>, current: &Path) -> Option<Self> {
        let key = name_key(current)?;
        let idx = entries.iter().position(|p| name_key(p).as_deref() == Some(key.as_str()))?;
        Some(Folder { entries, current: idx })
    }

    /// Number of sibling images in the folder.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 1-based position of the current image, for the "3 / 27" status read-out.
    pub fn position(&self) -> usize {
        self.current + 1
    }

    /// Move the cursor by `delta` (wrapping at both ends) and return the new current path.
    /// `entries` is non-empty by construction, so the wrap is always well-defined.
    pub fn advance(&mut self, delta: isize) -> PathBuf {
        let n = self.entries.len() as isize;
        self.current = (self.current as isize + delta).rem_euclid(n) as usize;
        self.entries[self.current].clone()
    }
}

/// Case-folded file name, the key used to both match the open image into the scan and to sort.
fn name_key(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase())
}

/// Compare two file names the way a file manager does: case-insensitively, with embedded digit
/// runs compared by numeric value so `img2` sorts before `img10` (not lexicographically, which
/// would put `img10` first). Ties on value fall back to longer-run-last so `img1`/`img01` are
/// ordered deterministically.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let na = leading_digits(&mut ai);
                    let nb = leading_digits(&mut bi);
                    // Compare by value: strip leading zeros, then longer run = larger number.
                    let va = na.trim_start_matches('0');
                    let vb = nb.trim_start_matches('0');
                    let ord = va.len().cmp(&vb.len()).then_with(|| va.cmp(vb));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    // Equal value (e.g. "1" vs "01"): order by raw run length for stability.
                    if na.len() != nb.len() {
                        return na.len().cmp(&nb.len());
                    }
                } else {
                    let la = ca.to_ascii_lowercase();
                    let lb = cb.to_ascii_lowercase();
                    if la != lb {
                        return la.cmp(&lb);
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// Consume and return the maximal run of ASCII digits at the front of `it`.
fn leading_digits(it: &mut Peekable<Chars>) -> String {
    let mut s = String::new();
    while let Some(&c) = it.peek() {
        if c.is_ascii_digit() {
            s.push(c);
            it.next();
        } else {
            break;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_image_is_case_insensitive() {
        assert!(is_supported_image(Path::new("a.png")));
        assert!(is_supported_image(Path::new("a.PNG")));
        assert!(is_supported_image(Path::new(r"C:\x\photo.JpEg")));
        assert!(is_supported_image(Path::new("a.avif")));
        assert!(!is_supported_image(Path::new("a.txt")));
        assert!(!is_supported_image(Path::new("noext")));
    }

    #[test]
    fn natural_sort_orders_numbers_by_value() {
        let mut names = vec!["img10.png", "img2.png", "img1.png", "img12.png"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["img1.png", "img2.png", "img10.png", "img12.png"]);
    }

    #[test]
    fn natural_sort_is_case_insensitive() {
        let mut names = vec!["Banana.jpg", "apple.jpg", "cherry.JPG"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["apple.jpg", "Banana.jpg", "cherry.JPG"]);
    }

    #[test]
    fn cursor_finds_current_and_wraps() {
        let entries = vec![
            PathBuf::from(r"C:\d\a.png"),
            PathBuf::from(r"C:\d\b.png"),
            PathBuf::from(r"C:\d\c.png"),
        ];
        // Match is case-insensitive on the file name.
        let mut f = Folder::new(entries, Path::new(r"C:\d\B.PNG")).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(f.position(), 2);
        assert_eq!(f.advance(1), PathBuf::from(r"C:\d\c.png"));
        assert_eq!(f.position(), 3);
        assert_eq!(f.advance(1), PathBuf::from(r"C:\d\a.png")); // wraps past the end
        assert_eq!(f.position(), 1);
        assert_eq!(f.advance(-1), PathBuf::from(r"C:\d\c.png")); // wraps past the start
        assert_eq!(f.position(), 3);
    }

    #[test]
    fn cursor_none_when_current_absent() {
        let entries = vec![PathBuf::from(r"C:\d\a.png")];
        assert!(Folder::new(entries, Path::new(r"C:\d\missing.png")).is_none());
    }
}
