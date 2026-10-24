//! The installer's way into the Explorer thumbnail registration (Windows).
//!
//! `fire.exe --set-thumbnails "tga,psd,exr"` makes Fire the thumbnail provider for exactly those
//! formats (the ids of `fire_thumbs::formats::GROUPS`; an empty list switches them all off), and
//! `fire.exe --remove-thumbnails` undoes all of it, putting back whatever Fire displaced. The
//! installer runs the first after copying the files and the second on uninstall, so the registry
//! logic exists once — in `fire_thumbs::registry`, which Settings ▸ Thumbnails also drives.
//!
//! These are maintenance commands, not launches: they return before the instance socket, the GPU
//! or a window exist, and their only output is the exit code — 0 done, 1 failed, 2 not understood.

use std::ffi::OsString;

use fire_thumbs::{formats, registry};

/// Handle a thumbnail command line, returning its exit code — or `None` when `args` (everything
/// after the program name) is not one, and Fire should launch as usual.
pub fn run_command(args: &[OsString]) -> Option<i32> {
    match args.first()?.to_str()? {
        "--set-thumbnails" => Some(match args.get(1).and_then(|a| a.to_str()) {
            Some(list) => set(list),
            None => 2,
        }),
        "--remove-thumbnails" => Some(report(registry::remove_all())),
        _ => None,
    }
}

/// Make Fire Explorer's thumbnail provider for exactly the formats `ids` — every other one off —
/// using the `fire_shellext.dll` beside this executable. What both `--set-thumbnails` and
/// Settings ▸ Thumbnails' Apply come down to.
pub fn set_formats(ids: &[&str]) -> Result<(), String> {
    // Switching everything off needs no DLL; switching anything on needs the one beside us.
    let dll = match registry::dll_beside_exe() {
        Some(d) => d,
        None if ids.is_empty() => registry::DLL_NAME.into(),
        None => return Err(format!("{} is not next to fire.exe", registry::DLL_NAME)),
    };
    registry::set_enabled(ids, &dll)
}

fn set(list: &str) -> i32 {
    let (ids, unknown) = formats::parse_ids(list);
    // A typo must not quietly switch a format off: refuse the whole list.
    if !unknown.is_empty() {
        eprintln!("fire: unknown thumbnail format(s): {}", unknown.join(", "));
        return 2;
    }
    report(set_formats(&ids))
}

fn report(r: Result<(), String>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fire: thumbnail registration: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    /// An image path, or no arguments at all, is a launch — never swallowed as a command.
    #[test]
    fn ordinary_launches_are_not_commands() {
        assert_eq!(run_command(&args(&[])), None);
        assert_eq!(run_command(&args(&[r"C:\pics\a.png"])), None);
    }

    /// Refused before anything is written: a missing list, and a list with a typo in it.
    #[test]
    fn malformed_commands_are_refused() {
        assert_eq!(run_command(&args(&["--set-thumbnails"])), Some(2));
        assert_eq!(
            run_command(&args(&["--set-thumbnails", "tga,pnng"])),
            Some(2)
        );
    }
}
