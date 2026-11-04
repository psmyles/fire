//! `fire-cli info`: what Fire's decoder sees in each image, as the rules measure it.

use std::path::PathBuf;

use crate::console::{say, warn};
use crate::facts;
use crate::messages as m;

/// `true` when every file could be read.
pub fn run(files: &[PathBuf]) -> bool {
    let mut ok = true;
    for (index, path) in files.iter().enumerate() {
        if index > 0 {
            say("");
        }
        say(&path.display().to_string());
        let facts = match facts::gather(path) {
            Ok(facts) => facts,
            Err(problem) => {
                warn(&format!("  {}", m::not_checked(&problem)));
                ok = false;
                continue;
            }
        };
        let row = |name: &str, value: &str| say(&m::info_row(name, value));
        row(m::INFO_FORMAT, &facts.format.name);
        row(
            m::INFO_SIZE,
            &m::info_size(facts.width, facts.height, facts.surfaces),
        );
        row(
            m::INFO_DEPTH,
            &m::info_depth(facts.bit_depth, facts.float, facts.palette),
        );
        match &facts.pixels {
            Some(pixels) => {
                row(
                    m::INFO_CHANNELS,
                    &m::info_channels(pixels.channels, Some(pixels.alpha_opaque)),
                );
                let profile = match &pixels.icc {
                    Some(icc) => m::info_profile(icc.description.as_deref(), icc.srgb),
                    None => m::info_no_profile(),
                };
                row(m::INFO_PROFILE, &profile);
            }
            None => row(m::INFO_CHANNELS, &m::info_no_pixels()),
        }
        if facts.no_merged_image {
            row(m::INFO_MERGED, &m::info_merged_missing());
        }
        row(m::INFO_FILE_SIZE, &m::file_size(facts.file_size));
        let generation = if facts.generation.is_empty() {
            m::info_none()
        } else {
            facts.generation.join(", ")
        };
        row(m::INFO_GENERATION, &generation);
    }
    ok
}
