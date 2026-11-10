//! What one image is, as the rules measure it: gathered once per file, from fire-decode and from
//! the file's own bytes.
//!
//! The image is decoded the way Asset Check needs it (AC12), with fire-decode's `inspect()` setup:
//! at full size, never downscaled, with no ICC conversion and no EXIF rotation, so the alpha scan
//! sees every pixel as stored, the colour-profile test sees the profile the file carries, and the
//! size is the stored one Unity imports.
//!
//! Two kinds of Photoshop file can't be decoded, but are still worth checking: a PSB (fire-decode
//! reads PSD only), and a PSD with no merged image. For those, the size and depth come from the
//! file's header, and the rules that need pixels don't run.

use std::path::Path;

use fire_decode::{DecodeError, DecodeOptions};

use crate::messages as m;
use crate::metadata;

/// The facts about one image.
#[derive(Clone, Debug, PartialEq)]
pub struct Facts {
    pub file_size: u64,
    /// The format the content is in, whatever the file's extension says.
    pub format: Format,
    /// One surface's size: for a cubemap, array or volume, a face's, not the sheet fire-decode
    /// tiles them into.
    pub width: u32,
    pub height: u32,
    /// How many surfaces, and what kind, when there is more than one.
    pub surfaces: Option<(u32, &'static str)>,
    /// Bits per channel of the source.
    pub bit_depth: u8,
    pub float: bool,
    pub palette: bool,
    /// What only a decode can tell: `None` for a PSB, or a PSD with no merged image.
    pub pixels: Option<Pixels>,
    /// A PSD (or PSB) whose merged image is missing.
    pub no_merged_image: bool,
    pub psb: bool,
    /// Generation metadata found, as the finding lists it.
    pub generation: Vec<String>,
}

/// The facts a decode gives.
#[derive(Clone, Debug, PartialEq)]
pub struct Pixels {
    /// 1 grey, 2 grey + alpha, 3 RGB, 4 RGBA.
    pub channels: u8,
    /// Whether a declared alpha channel is entirely opaque.
    pub alpha_opaque: bool,
    pub icc: Option<Icc>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Icc {
    /// The profile's description, e.g. `Display P3`.
    pub description: Option<String>,
    pub srgb: bool,
}

/// An image format: the id `texture.format` lists and compares, and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Format {
    /// Lowercase, as an extension is written: `png`, `jpg`, `tiff`, `psd`, `psb`, `exr`, ...
    pub id: String,
    /// As people say it: `PNG`, `JPEG`, `OpenEXR`.
    pub name: String,
}

impl Format {
    fn new(id: &str, name: &str) -> Format {
        Format {
            id: id.to_owned(),
            name: name.to_owned(),
        }
    }

    /// The format of a decoded image, from the name fire-decode gives it.
    fn of_decoded(source_format: &str) -> Format {
        let id = match source_format {
            name if name.starts_with("DDS") => "dds",
            "JPEG" => "jpg",
            "JPEG XL" => "jxl",
            "OpenEXR" => "exr",
            "Radiance HDR" => "hdr",
            "Farbfeld" => "ff",
            other => {
                return Format {
                    id: other.to_lowercase(),
                    name: other.to_owned(),
                };
            }
        };
        let name = if source_format.starts_with("DDS") {
            "DDS"
        } else {
            source_format
        };
        Format::new(id, name)
    }
}

/// The same name for the same format however it is spelt: `jpeg` is `jpg`, `tif` is `tiff`.
pub fn normalize_format(id: &str) -> String {
    match id.to_lowercase().as_str() {
        "jpeg" | "jpe" | "jfif" => "jpg".to_owned(),
        "tif" => "tiff".to_owned(),
        "targa" => "tga".to_owned(),
        other => other.to_owned(),
    }
}

/// Gathers the facts about the image at `path`. `Err` is why it couldn't be checked.
pub fn gather(path: &Path) -> Result<Facts, String> {
    let bytes = std::fs::read(path).map_err(|e| m::file_unreadable(&e.to_string()))?;
    let generation = metadata::scan(&bytes);
    let file_size = bytes.len() as u64;

    let header = photoshop_header(&bytes);
    if let Some(header) = header.as_ref().filter(|h| h.psb) {
        // fire-decode reads PSD only; a PSB is checked on what its header says.
        return Ok(from_header(header, file_size, false, generation));
    }

    let ext = path.extension().and_then(|e| e.to_str());
    // Full size, no ICC conversion and no EXIF rotation: the file as stored, which is what Unity
    // imports (fire-decode's `inspect()` setup).
    let image = match fire_decode::decode(&bytes, ext, &DecodeOptions::inspect()) {
        Ok(image) => image,
        Err(DecodeError::NoMergedImage) => match header {
            Some(header) => return Ok(from_header(&header, file_size, true, generation)),
            None => return Err(m::decode_failed(&DecodeError::NoMergedImage.to_string())),
        },
        Err(error) => return Err(m::decode_failed(&error.to_string())),
    };

    let (width, height, surfaces) = match image.layout {
        Some(layout) => (
            image.width / layout.cols.max(1),
            image.height / layout.rows.max(1),
            Some((layout.frames, layout.kind.label())),
        ),
        None => (image.width, image.height, None),
    };
    let icc = image.icc.as_deref().map(|profile| Icc {
        description: icc_description(profile),
        srgb: fire_decode::icc_is_srgb(profile),
    });
    Ok(Facts {
        file_size,
        format: Format::of_decoded(image.source.name),
        width,
        height,
        surfaces,
        bit_depth: image.source.bit_depth,
        float: image.format.is_hdr(),
        palette: image.source.palette,
        pixels: Some(Pixels {
            channels: image.source.channels,
            alpha_opaque: image.alpha_opaque,
            icc,
        }),
        no_merged_image: false,
        psb: false,
        generation,
    })
}

/// What a Photoshop header says: `8BPS`, version (1 PSD, 2 PSB), 6 reserved bytes, channels,
/// height, width, depth, colour mode.
struct PhotoshopHeader {
    psb: bool,
    width: u32,
    height: u32,
    depth: u16,
}

fn photoshop_header(bytes: &[u8]) -> Option<PhotoshopHeader> {
    let header = bytes.get(..26)?;
    if &header[..4] != b"8BPS" {
        return None;
    }
    let u16_at = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
    let u32_at = |at: usize| u32::from_be_bytes(header[at..at + 4].try_into().unwrap());
    Some(PhotoshopHeader {
        psb: u16_at(4) == 2,
        height: u32_at(14),
        width: u32_at(18),
        depth: u16_at(22),
    })
}

fn from_header(
    header: &PhotoshopHeader,
    file_size: u64,
    no_merged_image: bool,
    generation: Vec<String>,
) -> Facts {
    let format = if header.psb {
        Format::new("psb", "PSB")
    } else {
        Format::new("psd", "PSD")
    };
    Facts {
        file_size,
        format,
        width: header.width,
        height: header.height,
        surfaces: None,
        bit_depth: header.depth.min(255) as u8,
        float: header.depth == 32,
        palette: false,
        pixels: None,
        no_merged_image,
        psb: header.psb,
        generation,
    }
}

/// The profile's own name for itself (`desc`), if lcms2 can read one.
fn icc_description(profile: &[u8]) -> Option<String> {
    use lcms2::{InfoType, Locale, Profile};
    std::panic::catch_unwind(|| {
        let profile = Profile::new_icc(profile).ok()?;
        let text = profile.info(InfoType::Description, Locale::none())?;
        let text = text.trim().to_owned();
        (!text.is_empty()).then_some(text)
    })
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_formats_get_the_ids_extensions_use() {
        let id = |name: &str| Format::of_decoded(name).id;
        assert_eq!(id("PNG"), "png");
        assert_eq!(id("JPEG"), "jpg");
        assert_eq!(id("TIFF"), "tiff");
        assert_eq!(id("DDS BC7"), "dds");
        assert_eq!(Format::of_decoded("DDS BC7").name, "DDS");
        assert_eq!(id("OpenEXR"), "exr");
        assert_eq!(id("Radiance HDR"), "hdr");
        assert_eq!(id("WebP"), "webp");
        assert_eq!(id("AVIF"), "avif");
    }

    #[test]
    fn spellings_of_one_format_are_one_id() {
        assert_eq!(normalize_format("JPEG"), "jpg");
        assert_eq!(normalize_format("tif"), "tiff");
        assert_eq!(normalize_format("PNG"), "png");
    }

    /// A JPEG whose EXIF says "rotate 90° clockwise" reports its stored size, which is what Unity
    /// imports, not the upright size a viewer shows. Before fire-decode's `inspect()` setup it
    /// reported the upright one.
    #[test]
    fn a_rotated_jpeg_reports_its_stored_size() {
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(16, 8))
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        // APP1: "Exif\0\0", a little-endian TIFF header, one IFD entry: Orientation (a SHORT) = 6.
        let mut app1 = b"Exif\0\0II\x2a\x00\x08\x00\x00\x00\x01\x00".to_vec();
        app1.extend_from_slice(&0x0112u16.to_le_bytes());
        app1.extend_from_slice(&3u16.to_le_bytes());
        app1.extend_from_slice(&1u32.to_le_bytes());
        app1.extend_from_slice(&6u32.to_le_bytes());
        app1.extend_from_slice(&0u32.to_le_bytes());
        let mut bytes = jpeg[..2].to_vec();
        bytes.extend_from_slice(&[0xFF, 0xE1]);
        bytes.extend_from_slice(&(app1.len() as u16 + 2).to_be_bytes());
        bytes.extend_from_slice(&app1);
        bytes.extend_from_slice(&jpeg[2..]);

        let path =
            std::env::temp_dir().join(format!("fire-cli-rotated-{}.jpg", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let facts = gather(&path);
        let _ = std::fs::remove_file(&path);
        let facts = facts.unwrap();
        assert_eq!((facts.width, facts.height), (16, 8));
        assert_eq!(facts.format.id, "jpg");
    }

    #[test]
    fn a_psb_header_is_read_without_decoding() {
        let mut psb = b"8BPS\x00\x02".to_vec();
        psb.extend_from_slice(&[0; 6]);
        psb.extend_from_slice(&3u16.to_be_bytes());
        psb.extend_from_slice(&40_000u32.to_be_bytes()); // height
        psb.extend_from_slice(&30_000u32.to_be_bytes()); // width
        psb.extend_from_slice(&16u16.to_be_bytes());
        psb.extend_from_slice(&3u16.to_be_bytes());
        let header = photoshop_header(&psb).unwrap();
        assert!(header.psb);
        let facts = from_header(&header, 99, false, Vec::new());
        assert_eq!(
            (facts.width, facts.height, facts.bit_depth),
            (30_000, 40_000, 16)
        );
        assert_eq!(facts.format.id, "psb");
        assert!(facts.pixels.is_none());
    }
}
