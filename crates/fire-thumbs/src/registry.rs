//! Turning Fire's thumbnails on and off for a format — per user, in `HKCU`, like the rest of Fire's
//! registration (no admin, no machine-wide state).
//!
//! **Where Explorer looks.** For a file's thumbnail provider the shell asks, in order: the file's
//! *ProgID* (the user's chosen default app, else the extension's default), then the *extension* key,
//! then `SystemFileAssociations` (where Windows' own photo thumbnailer sits). The first that names a
//! provider wins — measured, not taken from the documentation: with a provider under both a ProgID
//! and its extension, the ProgID's is the one that runs. So Fire writes:
//!
//! * `CLSID\{Fire}` — the class: `InprocServer32` names `fire_shellext.dll` beside `fire.exe`.
//!   Written whenever any format is on, removed when the last one goes off. No
//!   `DisableProcessIsolation`: Explorer runs the provider in its COM surrogate, so a file that
//!   crashes the decoder takes down a throwaway `dllhost.exe`, never Explorer.
//! * `.<ext>\ShellEx\{IThumbnailProvider}` = `{Fire}` — one per extension of every format that is
//!   on. Ahead of `SystemFileAssociations`, so it beats Windows' own thumbnailer.
//! * `<ProgID>\ShellEx\{IThumbnailProvider}` = `{Fire}` — only where a ProgID standing in front of
//!   an extension of a format that is on has a provider of its own, which would otherwise win.
//!   Windows ships two (`jpegfile`, `icofile`); other programs may add theirs. Written only when
//!   *every* extension that ProgID fronts is a format that is on: a program's catch-all ProgID
//!   would otherwise route files Fire cannot read to Fire, and cost them their thumbnails.
//!
//! All of these go under `HKCU\Software\Classes`, so a provider registered machine-wide is
//! *shadowed*, not overwritten, and shows through again by itself once Fire's value is gone. Only a
//! value this user already had needs saving; `Software\Fire\Thumbnails` keeps them — one value per
//! extension (`.tga`), and one per ProgID under `ProgIDs` (empty when there was nothing to save) —
//! and Fire puts them back when it lets go.
//!
//! Every change ends with `SHCNE_ASSOCCHANGED`, which is how Explorer learns to stop reading what it
//! had cached about the extension.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW,
    RegEnumValueW, RegGetValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW, HKEY,
    HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

use crate::formats::GROUPS;

/// The file the provider ships as — `fire-shellext`'s cdylib, installed beside `fire.exe`.
pub const DLL_NAME: &str = "fire_shellext.dll";

/// The `ShellEx` slot Explorer looks a thumbnail provider up under: `IThumbnailProvider`'s IID.
const THUMBNAIL_SLOT: &str = "{e357fccd-a995-4576-b01f-234630154e96}";

/// Where the providers Fire replaced on extension keys are kept, one value per extension.
const REPLACED_KEY: &str = r"Software\Fire\Thumbnails";

/// Where the ProgIDs Fire shadowed are kept: value = ProgID, data = the provider this user had on it
/// before, or empty when there was none.
const PROGIDS_KEY: &str = r"Software\Fire\Thumbnails\ProgIDs";

/// Where the user's choice of default app for `.ext` is recorded.
fn user_choice_key(ext: &str) -> String {
    format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{ext}\UserChoice")
}

/// `fire_shellext.dll` beside the running executable, if it is there. The installer puts the two
/// side by side, and so does `cargo build`, so this is how a dev build registers itself too.
pub fn dll_beside_exe() -> Option<PathBuf> {
    let dll = std::env::current_exe().ok()?.with_file_name(DLL_NAME);
    dll.is_file().then_some(dll)
}

/// The ids of the formats Fire currently provides thumbnails for. A format counts as on when any of
/// its extensions points at Fire — so another program taking over one of a format's extensions does
/// not make it read as off, and so get switched back on over it at the next Apply.
pub fn enabled() -> Vec<&'static str> {
    let ours = crate::clsid_braced();
    GROUPS
        .iter()
        .filter(|g| g.exts.iter().any(|e| is_ours(&handler(e), &ours)))
        .map(|g| g.id)
        .collect()
}

/// Make Fire the thumbnail provider for exactly the formats in `ids` — on for those, off (with the
/// replaced provider restored) for every other — with the class pointing at `dll`. Ids that name no
/// format are ignored; the callers parse and report them.
pub fn set_enabled(ids: &[&str], dll: &Path) -> Result<(), String> {
    let on: Vec<&str> = GROUPS
        .iter()
        .filter(|g| ids.contains(&g.id))
        .flat_map(|g| g.exts.iter().copied())
        .collect();
    let mut errs = Vec::new();
    // The class first and gone last, so nothing ever names a class that is not there.
    if !on.is_empty() {
        if let Err(e) = write_class(dll) {
            errs.push(format!("registering the provider class: {e}"));
        }
    }
    for g in GROUPS {
        for ext in g.exts {
            let r = if on.contains(ext) {
                take(ext)
            } else {
                release(ext)
            };
            if let Err(e) = r {
                errs.push(format!(".{ext}: {e}"));
            }
        }
    }
    errs.extend(reconcile_progids(&on, |_| true));
    if on.is_empty() {
        let _ = delete_tree(&class_key());
        let _ = prune(PROGIDS_KEY);
        let _ = prune(REPLACED_KEY);
        let _ = prune(r"Software\Fire");
    }
    notify();
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

/// Undo all of it — every format off, the class and the bookkeeping gone. What uninstall runs.
pub fn remove_all() -> Result<(), String> {
    // `set_enabled` with nothing on never reads the DLL path.
    set_enabled(&[], Path::new(DLL_NAME))
}

fn class_key() -> String {
    format!(r"Software\Classes\CLSID\{}", crate::clsid_braced())
}

fn slot_key(ext: &str) -> String {
    format!(r"Software\Classes\.{ext}\ShellEx\{THUMBNAIL_SLOT}")
}

fn write_class(dll: &Path) -> Result<(), String> {
    let class = class_key();
    set_string(&class, "", "Fire Thumbnail Provider")?;
    let server = format!(r"{class}\InprocServer32");
    set_string(&server, "", &dll.to_string_lossy())?;
    set_string(&server, "ThreadingModel", "Apartment")
}

/// The per-user provider for `.ext` — the only kind this module writes or saves.
fn handler(ext: &str) -> Option<String> {
    get_string(&slot_key(ext), "")
}

fn is_ours(v: &Option<String>, ours: &str) -> bool {
    v.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(ours))
}

/// Point `.ext` at Fire, saving whatever per-user provider it pointed at before.
fn take(ext: &str) -> Result<(), String> {
    let ours = crate::clsid_braced();
    let name = format!(".{ext}");
    match handler(ext) {
        Some(v) if v.eq_ignore_ascii_case(&ours) => return Ok(()),
        Some(prev) => set_string(REPLACED_KEY, &name, &prev)?,
        // Nothing to put back later; drop any record left from an earlier round.
        None => delete_value(REPLACED_KEY, &name)?,
    }
    set_string(&slot_key(ext), "", &ours)
}

/// Let go of `.ext`: put the saved provider back, or remove Fire's entry when there was none. An
/// extension some other program has taken since is left to it — the saved record is stale then,
/// and is dropped.
fn release(ext: &str) -> Result<(), String> {
    let name = format!(".{ext}");
    if is_ours(&handler(ext), &crate::clsid_braced()) {
        match get_string(REPLACED_KEY, &name) {
            Some(prev) => set_string(&slot_key(ext), "", &prev)?,
            None => {
                delete_tree(&slot_key(ext))?;
                // The keys Fire may have created on the way down, if nothing else lives in them —
                // never `.ext` itself while an association or anything else is there.
                prune(&format!(r"Software\Classes\.{ext}\ShellEx"))?;
                prune(&format!(r"Software\Classes\.{ext}"))?;
            }
        }
    }
    delete_value(REPLACED_KEY, &name)
}

// --- ProgIDs -----------------------------------------------------------------------------------

/// The ProgIDs that can stand in front of `.ext`: the user's choice of default app, and the
/// extension's own default. Fire's own (`Fire.*`) never carry a provider, so they are left out.
fn fronting(ext: &str) -> Vec<String> {
    let mut v = Vec::new();
    let candidates = [
        get(HKEY_CURRENT_USER, &user_choice_key(ext), "ProgId"),
        get(HKEY_CLASSES_ROOT, &format!(".{ext}"), ""),
    ];
    for p in candidates.into_iter().flatten() {
        let fire = p.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("Fire."));
        if !p.is_empty() && !fire && !v.iter().any(|q: &String| q.eq_ignore_ascii_case(&p)) {
            v.push(p);
        }
    }
    v
}

/// The provider `progid` hands out with Fire's shadow taken away — what would win over Fire's
/// extension entries. `recorded` is what Fire saved when it shadowed the ProgID, if it did.
fn foreign_provider(progid: &str, recorded: Option<&str>) -> Option<String> {
    let slot = format!(r"{progid}\ShellEx\{THUMBNAIL_SLOT}");
    let v = match recorded {
        Some(prev) if !prev.is_empty() => Some(prev.to_string()),
        // Shadowed with nothing per-user before: what is underneath is the machine's.
        Some(_) => get(HKEY_LOCAL_MACHINE, &format!(r"Software\Classes\{slot}"), ""),
        None => get(HKEY_CLASSES_ROOT, &slot, ""),
    };
    v.filter(|v| !v.eq_ignore_ascii_case(&crate::clsid_braced()))
}

/// Every extension (no dot, lower-case) whose default or chosen app is `progid`. Read from all of
/// `HKCR` and the user's choices — a few thousand keys, and only ever for a ProgID that actually
/// carries a provider, which is rare.
fn fronted_by(progid: &str, index: &[(String, String)]) -> Vec<String> {
    index
        .iter()
        .filter(|(_, p)| p.eq_ignore_ascii_case(progid))
        .map(|(e, _)| e.clone())
        .collect()
}

/// `(extension, ProgID)` for every default and every user choice on this machine.
fn progid_index() -> Vec<(String, String)> {
    let mut index = Vec::new();
    for name in subkeys(HKEY_CLASSES_ROOT, "") {
        if let Some(ext) = name.strip_prefix('.') {
            if let Some(p) = get(HKEY_CLASSES_ROOT, &name, "") {
                index.push((ext.to_ascii_lowercase(), p));
            }
        }
    }
    let file_exts = r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts";
    for name in subkeys(HKEY_CURRENT_USER, file_exts) {
        if let Some(ext) = name.strip_prefix('.') {
            if let Some(p) = get(HKEY_CURRENT_USER, &user_choice_key(ext), "ProgId") {
                index.push((ext.to_ascii_lowercase(), p));
            }
        }
    }
    index
}

/// Bring the ProgID shadows in line with the extensions in `on`: shadow every ProgID that fronts
/// one of them with a provider of its own (and fronts nothing that is off), and let go of every
/// shadow no longer needed. Only recorded ProgIDs `in_scope` accepts are touched — everything, in
/// use; the tests' own, under test. Returns the errors.
fn reconcile_progids(on: &[&str], in_scope: impl Fn(&str) -> bool) -> Vec<String> {
    let recorded: Vec<(String, String)> = values(PROGIDS_KEY)
        .into_iter()
        .filter(|(p, _)| in_scope(p))
        .collect();
    let saved = |p: &str| {
        recorded
            .iter()
            .find(|(r, _)| r.eq_ignore_ascii_case(p))
            .map(|(_, prev)| prev.as_str())
    };
    let index = OnceCell::new();
    let mut need: Vec<String> = Vec::new();
    for ext in on {
        for p in fronting(ext) {
            if need.iter().any(|n| n.eq_ignore_ascii_case(&p))
                || foreign_provider(&p, saved(&p)).is_none()
            {
                continue;
            }
            let fronts = fronted_by(&p, index.get_or_init(progid_index));
            if fronts.iter().all(|e| on.contains(&e.as_str())) {
                need.push(p);
            }
        }
    }

    let mut errs = Vec::new();
    for (p, prev) in &recorded {
        if !need.iter().any(|n| n.eq_ignore_ascii_case(p)) {
            if let Err(e) = release_progid(p, prev) {
                errs.push(format!("{p}: {e}"));
            }
        }
    }
    for p in &need {
        if saved(p).is_none() {
            if let Err(e) = take_progid(p) {
                errs.push(format!("{p}: {e}"));
            }
        }
    }
    errs
}

fn progid_slot(progid: &str) -> String {
    format!(r"Software\Classes\{progid}\ShellEx\{THUMBNAIL_SLOT}")
}

/// Shadow `progid`'s provider with Fire's, saving the per-user value it had (or that it had none).
fn take_progid(progid: &str) -> Result<(), String> {
    let ours = crate::clsid_braced();
    let prev = get_string(&progid_slot(progid), "")
        .filter(|v| !v.eq_ignore_ascii_case(&ours))
        .unwrap_or_default();
    set_string(PROGIDS_KEY, progid, &prev)?;
    set_string(&progid_slot(progid), "", &ours)
}

/// Lift Fire's shadow off `progid`: the saved value back, or Fire's entry (and any key Fire created
/// for it) gone. A value something else has written since is left alone.
fn release_progid(progid: &str, prev: &str) -> Result<(), String> {
    let slot = progid_slot(progid);
    if is_ours(&get_string(&slot, ""), &crate::clsid_braced()) {
        if prev.is_empty() {
            delete_tree(&slot)?;
            prune(&format!(r"Software\Classes\{progid}\ShellEx"))?;
            prune(&format!(r"Software\Classes\{progid}"))?;
        } else {
            set_string(&slot, "", prev)?;
        }
    }
    delete_value(PROGIDS_KEY, progid)
}

fn notify() {
    // SAFETY: no item pointers — ASSOCCHANGED takes none.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

// --- registry leaves ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(e: WIN32_ERROR) -> Result<(), String> {
    if e == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(windows::core::Error::from(e.to_hresult()).message())
    }
}

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key this module opened and nothing else holds.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn create(path: &str) -> Result<Key, String> {
    let p = wide(path);
    let mut k = HKEY::default();
    // SAFETY: `p` is NUL-terminated and outlives the call; `k` receives the handle.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(p.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut k,
            None,
        )
    })?;
    Ok(Key(k))
}

fn open(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> Option<Key> {
    let p = wide(path);
    let mut k = HKEY::default();
    // SAFETY: as in `create`.
    let e = unsafe { RegOpenKeyExW(root, PCWSTR(p.as_ptr()), None, access, &mut k) };
    (e == ERROR_SUCCESS).then_some(Key(k))
}

/// A string value under any root — `HKCR` to see what the shell sees, `HKLM` for what is under a
/// per-user shadow.
fn get(root: HKEY, path: &str, name: &str) -> Option<String> {
    let (p, n) = (wide(path), wide(name));
    let mut buf = [0u16; 512];
    let mut len = std::mem::size_of_val(&buf) as u32;
    // SAFETY: `buf` is `len` bytes; RRF_RT_REG_SZ guarantees a terminated string or an error.
    let e = unsafe {
        RegGetValueW(
            root,
            PCWSTR(p.as_ptr()),
            PCWSTR(n.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if e != ERROR_SUCCESS {
        return None;
    }
    let chars = (len as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..chars.min(buf.len())]))
}

fn get_string(path: &str, name: &str) -> Option<String> {
    get(HKEY_CURRENT_USER, path, name)
}

fn set_string(path: &str, name: &str, value: &str) -> Result<(), String> {
    let key = create(path)?;
    let (n, v) = (wide(name), wide(value));
    // SAFETY: REG_SZ data is the NUL-terminated UTF-16 bytes of `v`.
    check(unsafe {
        RegSetValueExW(
            key.0,
            PCWSTR(n.as_ptr()),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(v.as_ptr().cast(), v.len() * 2)),
        )
    })
}

/// The names of `path`'s subkeys (an empty path is the root itself).
fn subkeys(root: HKEY, path: &str) -> Vec<String> {
    let Some(key) = open(root, path, KEY_READ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut buf = [0u16; 256]; // a key name is at most 255 characters
    for i in 0.. {
        let mut len = buf.len() as u32;
        // SAFETY: `buf` holds `len` characters.
        let e = unsafe {
            RegEnumKeyExW(
                key.0,
                i,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if e != ERROR_SUCCESS {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    out
}

/// `(name, string data)` for every value under an `HKCU` key; non-string values are skipped.
fn values(path: &str) -> Vec<(String, String)> {
    let Some(key) = open(HKEY_CURRENT_USER, path, KEY_READ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut name = [0u16; 16384]; // the longest value name the registry allows
    for i in 0.. {
        let mut len = name.len() as u32;
        // SAFETY: `name` holds `len` characters; no data is read here.
        let e = unsafe {
            RegEnumValueW(
                key.0,
                i,
                Some(PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if e != ERROR_SUCCESS {
            break;
        }
        let n = String::from_utf16_lossy(&name[..len as usize]);
        if let Some(data) = get_string(path, &n) {
            out.push((n, data));
        }
    }
    out
}

/// Delete one value; a value (or key) that is not there is not an error.
fn delete_value(path: &str, name: &str) -> Result<(), String> {
    let Some(key) = open(HKEY_CURRENT_USER, path, KEY_SET_VALUE) else {
        return Ok(());
    };
    let n = wide(name);
    // SAFETY: `n` is NUL-terminated.
    let e = unsafe { RegDeleteValueW(key.0, PCWSTR(n.as_ptr())) };
    if e == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        check(e)
    }
}

/// Delete a key and everything under it; one that is not there is not an error.
fn delete_tree(path: &str) -> Result<(), String> {
    let p = wide(path);
    // SAFETY: `p` is NUL-terminated.
    let e = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr())) };
    if e == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(e)?;
    // RegDeleteTree empties the key but leaves the key itself.
    // SAFETY: as above.
    let e = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr())) };
    if e == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        check(e)
    }
}

/// Delete `path` if it holds nothing at all — no values (the default included) and no subkeys.
fn prune(path: &str) -> Result<(), String> {
    let Some(key) = open(HKEY_CURRENT_USER, path, KEY_READ) else {
        return Ok(());
    };
    let (mut subkeys, mut values) = (0u32, 0u32);
    // SAFETY: two out-counts; every other field is optional and passed as None.
    check(unsafe {
        RegQueryInfoKeyW(
            key.0,
            None,
            None,
            None,
            Some(&mut subkeys),
            None,
            None,
            Some(&mut values),
            None,
            None,
            None,
            None,
        )
    })?;
    drop(key);
    if subkeys == 0 && values == 0 {
        let p = wide(path);
        // SAFETY: `p` is NUL-terminated.
        check(unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr())) })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! These write to the real `HKCU`, so each uses extensions and ProgIDs of its own that nothing
    //! else could own, and cleans up after itself. They never touch the class key, a real format or
    //! a real ProgID. They share the bookkeeping keys, which `reset` prunes, so they take turns.
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    fn serial() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset(names: &[&str]) {
        for n in names {
            let _ = delete_tree(&format!(r"Software\Classes\{n}"));
            let _ = delete_value(REPLACED_KEY, n);
            let _ = delete_value(PROGIDS_KEY, n);
        }
        let _ = prune(PROGIDS_KEY);
        let _ = prune(REPLACED_KEY);
        let _ = prune(r"Software\Fire");
    }

    /// Take over an extension another per-user provider held, give it back, and find that provider
    /// restored and no trace of Fire left.
    #[test]
    fn take_and_release_restores_the_previous_provider() {
        let _serial = serial();
        let ext = "firetestthumb-restore";
        reset(&[".firetestthumb-restore"]);
        let other = "{11111111-2222-3333-4444-555555555555}";
        set_string(&slot_key(ext), "", other).unwrap();

        take(ext).unwrap();
        assert!(is_ours(&handler(ext), &crate::clsid_braced()));
        assert_eq!(
            get_string(REPLACED_KEY, &format!(".{ext}")).as_deref(),
            Some(other)
        );
        // Taking it twice must not save Fire as the provider to restore.
        take(ext).unwrap();
        assert_eq!(
            get_string(REPLACED_KEY, &format!(".{ext}")).as_deref(),
            Some(other)
        );

        release(ext).unwrap();
        assert_eq!(handler(ext).as_deref(), Some(other));
        assert_eq!(get_string(REPLACED_KEY, &format!(".{ext}")), None);
        reset(&[".firetestthumb-restore"]);
    }

    /// With nothing there before, releasing removes every key taking created.
    #[test]
    fn release_without_a_predecessor_leaves_no_keys() {
        let _serial = serial();
        let ext = "firetestthumb-clean";
        reset(&[".firetestthumb-clean"]);
        take(ext).unwrap();
        release(ext).unwrap();
        assert!(open(
            HKEY_CURRENT_USER,
            &format!(r"Software\Classes\.{ext}"),
            KEY_READ
        )
        .is_none());
        reset(&[".firetestthumb-clean"]);
    }

    /// Something else took the extension after Fire did: releasing leaves it alone.
    #[test]
    fn release_leaves_a_later_owner_alone() {
        let _serial = serial();
        let ext = "firetestthumb-later";
        reset(&[".firetestthumb-later"]);
        take(ext).unwrap();
        let later = "{99999999-8888-7777-6666-555555555555}";
        set_string(&slot_key(ext), "", later).unwrap();
        release(ext).unwrap();
        assert_eq!(handler(ext).as_deref(), Some(later));
        reset(&[".firetestthumb-later"]);
    }

    /// A ProgID with a provider of its own in front of an extension that is on gets Fire's shadow,
    /// which comes off again — previous provider restored — when the extension goes off. A ProgID
    /// that also fronts an extension that is *off* is left alone.
    #[test]
    fn progids_in_front_are_shadowed_and_restored() {
        let _serial = serial();
        let (a, b, c) = ("firetestprog-a", "firetestprog-b", "firetestprog-c");
        let (solo, shared) = ("FireTestProg.Solo", "FireTestProg.Shared");
        let all = [
            ".firetestprog-a",
            ".firetestprog-b",
            ".firetestprog-c",
            solo,
            shared,
        ];
        reset(&all);
        let other = "{11111111-2222-3333-4444-555555555555}";
        // .a is fronted by Solo; .b and .c are both fronted by Shared.
        set_string(&format!(r"Software\Classes\.{a}"), "", solo).unwrap();
        set_string(&format!(r"Software\Classes\.{b}"), "", shared).unwrap();
        set_string(&format!(r"Software\Classes\.{c}"), "", shared).unwrap();
        set_string(&progid_slot(solo), "", other).unwrap();
        set_string(&progid_slot(shared), "", other).unwrap();
        let ours = crate::clsid_braced();
        let test_scope = |p: &str| p.starts_with("FireTestProg.");

        // .a and .b on, .c off: Solo is shadowed; Shared also fronts .c, so it is not.
        assert!(reconcile_progids(&[a, b], test_scope).is_empty());
        assert!(is_ours(&get_string(&progid_slot(solo), ""), &ours));
        assert_eq!(get_string(PROGIDS_KEY, solo).as_deref(), Some(other));
        assert_eq!(get_string(&progid_slot(shared), "").as_deref(), Some(other));

        // With .c on too, Shared is shadowed as well.
        assert!(reconcile_progids(&[a, b, c], test_scope).is_empty());
        assert!(is_ours(&get_string(&progid_slot(shared), ""), &ours));

        // Everything off: both restored, nothing recorded.
        assert!(reconcile_progids(&[], test_scope).is_empty());
        assert_eq!(get_string(&progid_slot(solo), "").as_deref(), Some(other));
        assert_eq!(get_string(&progid_slot(shared), "").as_deref(), Some(other));
        assert_eq!(get_string(PROGIDS_KEY, solo), None);
        assert_eq!(get_string(PROGIDS_KEY, shared), None);
        reset(&all);
    }
}
