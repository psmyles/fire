//! Round-trip smoke test for the psd_sdk FFI: open a PSD, print its info, ICC presence,
//! and the first pixels. Run: `cargo run -p psd-sdk-sys --example psd_roundtrip <file.psd>`

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: psd_roundtrip <file.psd>");
            std::process::exit(2);
        }
    };
    let bytes = std::fs::read(&path).expect("failed to read PSD");
    match psd_sdk_sys::decode_psd(&bytes) {
        Ok(img) => {
            println!(
                "PSD {}x{}  channels={} bits/ch={}  icc={}",
                img.width,
                img.height,
                img.channels,
                img.bits_per_channel,
                img.icc
                    .as_ref()
                    .map(|v| format!("{} bytes", v.len()))
                    .unwrap_or("none".into())
            );
            // `rgba` is in the document's own depth — u8, native-endian u16, or f32 — so the
            // raw bytes are what this smoke test can honestly print.
            let n = img.rgba.len().min(16);
            println!("first RGBA bytes: {:?}", &img.rgba[..n]);
        }
        Err(e) => {
            eprintln!("decode failed: {e}");
            std::process::exit(1);
        }
    }
}
