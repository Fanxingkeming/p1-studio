//! WebView2 loader, carried inside the executable.
//!
//! Upstream `webview2-com-sys` links `WebView2Loader.dll` into the import table,
//! which forces the DLL to sit next to the .exe (or on PATH) or the process dies
//! before `main` runs. On the MSVC target upstream switches to
//! `WebView2LoaderStatic.lib`, but that needs the MSVC linker — this project
//! builds with the GNU toolchain, where that option does not exist.
//!
//! So the DLL bytes are embedded here instead and loaded on first use: they are
//! written once to a per-user cache directory and pulled in with `LoadLibraryW`.
//! The five loader entry points below are the only ones WRY uses, and they keep
//! their original names and signatures, so nothing upstream has to change.

use std::path::PathBuf;

#[cfg(target_arch = "x86_64")]
const DLL_BYTES: &[u8] = include_bytes!("../x64/WebView2Loader.dll");
#[cfg(target_arch = "x86")]
const DLL_BYTES: &[u8] = include_bytes!("../x86/WebView2Loader.dll");
#[cfg(target_arch = "aarch64")]
const DLL_BYTES: &[u8] = include_bytes!("../arm64/WebView2Loader.dll");

const DLL_NAME: &str = "WebView2Loader.dll";
const CACHE_VENDOR: &str = "MetaphyuniHUB";

extern "system" {
    fn LoadLibraryW(path: *const u16) -> *mut core::ffi::c_void;
    fn GetProcAddress(module: *mut core::ffi::c_void, name: *const u8) -> *const core::ffi::c_void;
    fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The directory the loader is unpacked into. Per-user, so it works from a
/// read-only install location too.
fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    Some(base.join(CACHE_VENDOR).join("bin"))
}

/// Absolute path of a usable loader DLL, unpacking it from the executable if
/// this is the first run. `None` means we could neither find nor write one.
fn ensure_dll() -> Option<PathBuf> {
    // Already unpacked and still the same payload? Reuse it.
    let dir = cache_dir()?;
    let target = dir.join(DLL_NAME);
    if let Ok(meta) = std::fs::metadata(&target) {
        if meta.len() == DLL_BYTES.len() as u64 {
            return Some(target);
        }
    }
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(&target, DLL_BYTES).ok()?;
    Some(target)
}

fn module() -> Option<*mut core::ffi::c_void> {
    // Already loaded under this name (e.g. shipped next to the exe)? Use it.
    let loaded = unsafe { GetModuleHandleW(wide(DLL_NAME).as_ptr()) };
    if !loaded.is_null() {
        return Some(loaded);
    }
    let path = ensure_dll()?;
    let handle = unsafe { LoadLibraryW(wide(&path.to_string_lossy()).as_ptr()) };
    if handle.is_null() {
        None
    } else {
        Some(handle)
    }
}

/// Resolve one loader export by name.
pub(crate) fn symbol(name: &str) -> Option<*const ()> {
    let handle = module()?;
    let mut bytes: Vec<u8> = name.as_bytes().to_vec();
    bytes.push(0);
    let ptr = unsafe { GetProcAddress(handle, bytes.as_ptr()) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr.cast::<()>())
    }
}

/// Human-readable location of the loader, for the diagnostics panel.
pub fn location() -> Option<String> {
    let dir = cache_dir()?;
    Some(dir.join(DLL_NAME).to_string_lossy().into_owned())
}

pub const EMBEDDED_SIZE: usize = DLL_BYTES.len();
