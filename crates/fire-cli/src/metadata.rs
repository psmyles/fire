//! Generation metadata: what an AI image generator leaves inside the file it writes
//! (`texture.generation_metadata`). fire-decode doesn't read it, so this does (AC12).
//!
//! Two kinds are recognised:
//! - **PNG text chunks** (`tEXt`, `zTXt`, `iTXt`) under the keys the common generators use:
//!   `parameters` (AUTOMATIC1111, Forge, SD.Next, Fooocus), `prompt` and `workflow` (ComfyUI),
//!   `invokeai_metadata` / `invokeai_graph` / `sd-metadata` / `Dream` (InvokeAI),
//!   `fooocus_scheme` (Fooocus), and a `Software` chunk naming NovelAI;
//! - **XMP** marking the image as made by generative AI: IPTC's digital source type
//!   `trainedAlgorithmicMedia` or `compositeWithTrainedAlgorithmicMedia`, as Midjourney and
//!   Adobe Firefly write it. XMP is plain text in every container, so it is found by searching
//!   the bytes, whatever the format; inside a PNG it can also be compressed, which the chunk
//!   reader handles.

/// PNG text keys generators write, compared without regard to case.
const GENERATOR_KEYS: &[&str] = &[
    "parameters",
    "prompt",
    "workflow",
    "invokeai_metadata",
    "invokeai_graph",
    "sd-metadata",
    "dream",
    "fooocus_scheme",
];

/// The tail of both IPTC digital source types for generative AI, as it appears in XMP.
const AI_SOURCE_TYPE: &[u8] = b"rainedAlgorithmicMedia";

const XMP_PNG_KEY: &str = "XML:com.adobe.xmp";

/// Inflated text larger than this is cut off: a prompt is never this long, and a crafted chunk
/// mustn't inflate without bound.
const MAX_TEXT: usize = 1 << 20;

/// What generation metadata `bytes` carries, as the finding lists it: `"parameters" text chunk`,
/// `XMP marking it as AI-generated`. Empty when there is none.
pub fn scan(bytes: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut ai_xmp = false;
    if let Some(chunks) = png_text(bytes) {
        for (key, text) in chunks {
            let lower = key.to_lowercase();
            if GENERATOR_KEYS.contains(&lower.as_str()) {
                push_unique(&mut found, format!("a \"{key}\" text chunk"));
            } else if lower == "software" && text.to_lowercase().contains("novelai") {
                push_unique(&mut found, "a \"Software: NovelAI\" text chunk".to_owned());
            } else if key == XMP_PNG_KEY {
                ai_xmp |= memchr::memmem::find(text.as_bytes(), AI_SOURCE_TYPE).is_some();
            }
        }
    }
    ai_xmp |= memchr::memmem::find(bytes, AI_SOURCE_TYPE).is_some();
    if ai_xmp {
        found.push("XMP marking it as made by generative AI".to_owned());
    }
    found
}

fn push_unique(found: &mut Vec<String>, item: String) {
    if !found.contains(&item) {
        found.push(item);
    }
}

/// Every text chunk of a PNG, as `(key, text)`, or `None` if `bytes` isn't a PNG. A chunk that
/// can't be read is skipped; a truncated file ends the walk.
fn png_text(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let mut rest = bytes.strip_prefix(b"\x89PNG\r\n\x1a\n")?;
    let mut out = Vec::new();
    while rest.len() >= 12 {
        let len = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
        let kind = &rest[4..8];
        let Some(data) = rest.get(8..8 + len) else {
            break;
        };
        match kind {
            b"tEXt" => out.extend(text_chunk(data)),
            b"zTXt" => out.extend(ztxt_chunk(data)),
            b"iTXt" => out.extend(itxt_chunk(data)),
            b"IEND" => break,
            _ => {}
        }
        // A truncated file ends the walk; what was found before it still counts.
        let Some(next) = rest.get(12 + len..) else {
            break;
        };
        rest = next;
    }
    Some(out)
}

/// `keyword\0text`, both Latin-1.
fn text_chunk(data: &[u8]) -> Option<(String, String)> {
    let (key, text) = split_nul(data)?;
    Some((latin1(key), latin1(text)))
}

/// `keyword\0 method compressed-text`.
fn ztxt_chunk(data: &[u8]) -> Option<(String, String)> {
    let (key, rest) = split_nul(data)?;
    let compressed = rest.get(1..)?;
    Some((latin1(key), latin1(&inflate(compressed)?)))
}

/// `keyword\0 flag method language\0 translated-keyword\0 text`, the text UTF-8 and compressed
/// when `flag` is 1.
fn itxt_chunk(data: &[u8]) -> Option<(String, String)> {
    let (key, rest) = split_nul(data)?;
    let (&flag, rest) = rest.split_first()?;
    let rest = rest.get(1..)?; // compression method
    let (_language, rest) = split_nul(rest)?;
    let (_translated, text) = split_nul(rest)?;
    let text = if flag == 1 {
        inflate(text)?
    } else {
        text.to_vec()
    };
    Some((latin1(key), String::from_utf8_lossy(&text).into_owned()))
}

fn split_nul(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = data.iter().position(|&b| b == 0)?;
    Some((&data[..at], &data[at + 1..]))
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    use miniz_oxide::inflate::{decompress_to_vec_zlib_with_limit, TINFLStatus};
    match decompress_to_vec_zlib_with_limit(data, MAX_TEXT) {
        Ok(text) => Some(text),
        // Over the limit: what fits is enough to look in.
        Err(error) if error.status == TINFLStatus::HasMoreOutput => Some(error.output),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_with(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in chunks {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(*kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&[0; 4]); // CRC: not checked here
        }
        out
    }

    fn zlib(text: &str) -> Vec<u8> {
        miniz_oxide::deflate::compress_to_vec_zlib(text.as_bytes(), 6)
    }

    #[test]
    fn generator_chunks_are_found_in_every_text_chunk_kind() {
        let mut ztxt = b"workflow\0\0".to_vec();
        ztxt.extend(zlib("{\"nodes\": []}"));
        let mut itxt = b"prompt\0\x01\0en\0\0".to_vec();
        itxt.extend(zlib("{\"3\": {}}"));
        let png = png_with(&[
            (b"tEXt", b"parameters\0a cat, steps: 20".to_vec()),
            (b"zTXt", ztxt),
            (b"iTXt", itxt),
            (b"tEXt", b"Title\0A crate".to_vec()),
        ]);
        assert_eq!(
            scan(&png),
            [
                "a \"parameters\" text chunk",
                "a \"workflow\" text chunk",
                "a \"prompt\" text chunk",
            ]
        );
    }

    #[test]
    fn novelai_is_known_by_its_software_chunk() {
        let png = png_with(&[(b"tEXt", b"Software\0NovelAI".to_vec())]);
        assert_eq!(scan(&png), ["a \"Software: NovelAI\" text chunk"]);
        let png = png_with(&[(b"tEXt", b"Software\0Adobe Photoshop".to_vec())]);
        assert!(scan(&png).is_empty());
    }

    #[test]
    fn ai_xmp_is_found_compressed_in_a_png_or_plain_in_any_file() {
        let xmp = "<x:xmpmeta><Iptc4xmpExt:DigitalSourceType>http://cv.iptc.org/newscodes/\
                   digitalsourcetype/trainedAlgorithmicMedia</Iptc4xmpExt:DigitalSourceType>";
        let mut itxt = b"XML:com.adobe.xmp\0\x01\0\0\0".to_vec();
        itxt.extend(zlib(xmp));
        assert_eq!(
            scan(&png_with(&[(b"iTXt", itxt)])),
            ["XMP marking it as made by generative AI"]
        );
        let mut jpeg = b"\xff\xd8\xff\xe1".to_vec();
        jpeg.extend_from_slice(xmp.replace("trained", "compositeWithTrained").as_bytes());
        assert_eq!(scan(&jpeg), ["XMP marking it as made by generative AI"]);
    }

    /// A file cut off mid-chunk still reports what came before the cut.
    #[test]
    fn a_truncated_png_keeps_what_was_found() {
        let mut png = png_with(&[(b"tEXt", b"parameters\0a cat".to_vec())]);
        png.extend_from_slice(&1000u32.to_be_bytes());
        png.extend_from_slice(b"IDAT\x00\x01");
        assert_eq!(scan(&png), ["a \"parameters\" text chunk"]);
    }

    #[test]
    fn a_plain_image_has_none() {
        assert!(scan(&png_with(&[(b"IHDR", vec![0; 13])])).is_empty());
        assert!(scan(b"not an image").is_empty());
    }
}
