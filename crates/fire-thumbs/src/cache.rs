//! Emptying Explorer's thumbnail cache.
//!
//! Explorer keeps every thumbnail it has made (the `thumbcache_*.db` files under
//! `%LOCALAPPDATA%\Microsoft\Windows\Explorer`) and asks a provider again only when the file
//! changes. So a new backdrop or exposure reaches thumbnails made from now on, and nothing already
//! cached — not until the cache is emptied.
//!
//! The files themselves are held open by Explorer and cannot simply be deleted. Disk Cleanup's
//! "Thumbnails" item can empty them anyway, and it is an ordinary COM object, `IEmptyVolumeCache`
//! from `thumbcache.dll`, registered under `VolumeCaches\Thumbnail Cache`. This drives it the way
//! Disk Cleanup does: `Initialize` → `GetSpaceUsed` → `Purge` → `Deactivate`.
//!
//! It empties the cache for **every** file type — there is no per-provider purge — and Explorer
//! then redraws thumbnails as folders are opened.

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows::Win32::UI::LegacyWindowsEnvironmentFeatures::{
    IEmptyVolumeCache, IEmptyVolumeCacheCallBack, EMPTY_VOLUME_CACHE_FLAGS,
};

/// `thumbcache.dll`'s cleanup handler: `VolumeCaches\Thumbnail Cache`'s default value.
const CLEANER: GUID = GUID::from_u128(0x889900c3_59f3_4c2f_ae21_a409ea01e605);

/// The handler's own registry key, which `Initialize` is handed as Disk Cleanup hands it.
const CLEANER_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\VolumeCaches\Thumbnail Cache";

/// Empty Explorer's thumbnail cache. Blocks until done — seconds at most, but call it off the UI
/// thread. Initializes COM on the calling thread for the duration.
pub fn clear() -> Result<(), String> {
    // SAFETY: balanced below whenever it succeeds (S_FALSE, already initialized, included). A thread
    // already in the multithreaded apartment gets RPC_E_CHANGED_MODE and keeps its apartment, which
    // serves the handler (`ThreadingModel = Both`) just as well.
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result = purge();
    if initialized {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result.map_err(|e| e.message())
}

fn purge() -> windows::core::Result<()> {
    // SAFETY: plain COM calls on an interface we own; the out-strings are freed as the contract asks.
    unsafe {
        let cleaner: IEmptyVolumeCache = CoCreateInstance(&CLEANER, None, CLSCTX_INPROC_SERVER)?;

        let key = wide(CLEANER_KEY);
        let mut hkey = HKEY::default();
        let opened = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        ) == ERROR_SUCCESS;

        // The volume the cache lives on: the drive of `%LOCALAPPDATA%`, which is where
        // `thumbcache_*.db` are.
        let volume = wide(&cache_volume());
        let (mut name, mut desc) = (PWSTR::null(), PWSTR::null());
        let mut flags = EMPTY_VOLUME_CACHE_FLAGS(0);
        let init = cleaner.Initialize(
            hkey,
            PCWSTR(volume.as_ptr()),
            &mut name,
            &mut desc,
            &mut flags,
        );
        CoTaskMemFree(Some(name.0 as *const _));
        CoTaskMemFree(Some(desc.0 as *const _));
        if opened {
            let _ = RegCloseKey(hkey);
        }
        init?;

        let mut used = 0u64;
        cleaner.GetSpaceUsed(&mut used, None::<&IEmptyVolumeCacheCallBack>)?;
        cleaner.Purge(u64::MAX, None::<&IEmptyVolumeCacheCallBack>)?;
        cleaner.Deactivate()?;
    }
    Ok(())
}

/// `C:\` for a `%LOCALAPPDATA%` on C:, and `C:\` when it cannot be told.
fn cache_volume() -> String {
    dirs::data_local_dir()
        .and_then(|p| {
            p.components().next().map(|c| {
                let mut s = c.as_os_str().to_string_lossy().into_owned();
                s.push('\\');
                s
            })
        })
        .unwrap_or_else(|| r"C:\".into())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
