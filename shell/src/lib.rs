//! `babylon_shell.dll`: Explorer integration for 3D models.
//!
//! * Preview handler (Preview pane, Alt+P): hosts WebView2 running the same Babylon.js
//!   viewer bundle as the desktop app, so every format the app opens is previewable.
//! * Thumbnail provider: renders geometry on the CPU (see [`render`]) for fast, GPU-free
//!   thumbnails of STL / OBJ / glTF / PLY / splat files, and of STEP / IGES / BREP files via
//!   OpenCascade compiled to WebAssembly (see [`model::cad`]).

// The MSVC linker notes that Dll* exports "should be PRIVATE"; harmless for a COM server.
#![allow(linker_messages)]

pub mod model;
pub mod render;

/// Per-user folder for the extension's data (WebView2 profile, log):
/// `%USERPROFILE%\AppData\LocalLow\BabylonViewer`. Explorer runs preview handlers in a
/// low-integrity prevhost.exe, which can only write under LocalLow. The path comes from the
/// shell, not the environment: COM surrogates may start without the user's environment.
#[cfg(windows)]
pub fn data_dir() -> Option<std::path::PathBuf> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppDataLow, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
    };
    let base = unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_LocalAppDataLow, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }?;
    Some(std::path::PathBuf::from(base).join("BabylonViewer"))
}

#[cfg(not(windows))]
pub fn data_dir() -> Option<std::path::PathBuf> {
    None
}

/// Per-user cache (compiled OpenCascade kernel): `%LOCALAPPDATA%\BabylonViewer\cache`.
/// Deliberately not under LocalLow: the cache holds native code, so lower-integrity processes
/// must not be able to write it.
#[cfg(windows)]
pub fn cache_dir() -> Option<std::path::PathBuf> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
    };
    let base = unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }?;
    Some(
        std::path::PathBuf::from(base)
            .join("BabylonViewer")
            .join("cache"),
    )
}

/// Outside Windows (development and tests), caching is opt-in via `BABYLON_SHELL_CACHE`.
#[cfg(not(windows))]
pub fn cache_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("BABYLON_SHELL_CACHE").map(std::path::PathBuf::from)
}

/// OpenCascade kernel for CAD thumbnails: `occt.wasm` next to this DLL (prepared by
/// scripts/build-occt.mjs). `BABYLON_OCCT_WASM` overrides it; tests use the build output.
pub fn occt_wasm_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("BABYLON_OCCT_WASM") {
        return Some(p.into());
    }
    #[cfg(test)]
    {
        let built =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/occt/occt.wasm");
        if built.is_file() {
            return Some(built);
        }
    }
    #[cfg(windows)]
    {
        let beside_dll = dll_path().with_file_name("occt.wasm");
        if beside_dll.is_file() {
            return Some(beside_dll);
        }
    }
    None
}

/// Appends a line to the file named by `BABYLON_SHELL_LOG`, or to `shell.log` in
/// [`data_dir`] if that file already exists (create it to turn logging on). For diagnosing
/// the extension inside Explorer's host processes.
pub fn log(message: &str) {
    let path = std::env::var_os("BABYLON_SHELL_LOG")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let p = data_dir()?.join("shell.log");
            p.is_file().then_some(p)
        });
    if let Some(path) = path {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "[{}] {message}", std::process::id());
        }
    }
}

/// Renders a model file's bytes to a premultiplied BGRA thumbnail. `format` falls back to
/// content sniffing; external resources (e.g. a .gltf's .bin) are not available.
pub fn render_bytes(
    bytes: &[u8],
    format: Option<model::Format>,
    size: u32,
) -> Option<render::Image> {
    let format = format.or_else(|| model::Format::sniff(bytes))?;
    let model = model::load(bytes, format, &|_| None)
        .map_err(|e| log(&format!("cannot read {format:?} model: {e}")))
        .ok()?;
    render::render(&model, size)
}

#[cfg(windows)]
mod preview;
#[cfg(windows)]
mod registry;
#[cfg(windows)]
mod thumbnail;

#[cfg(windows)]
pub use com::*;

#[cfg(windows)]
mod com {
    use std::{ffi::c_void, path::PathBuf, ptr::null_mut};

    use windows::{
        core::{implement, IUnknown, Interface, Ref, BOOL, GUID, HRESULT},
        Win32::{
            Foundation::{
                CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, HMODULE, S_FALSE, S_OK,
            },
            System::{
                Com::{IClassFactory, IClassFactory_Impl},
                LibraryLoader::{
                    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                },
            },
        },
    };

    use crate::{preview::PreviewHandler, registry, thumbnail::ThumbnailProvider};

    pub const CLSID_PREVIEW_HANDLER: GUID = GUID::from_u128(0x5b9e1d2a_7c43_4f8e_9a61_3d2b8c7e4f10);
    pub const CLSID_THUMBNAIL_PROVIDER: GUID =
        GUID::from_u128(0x8e4a6c1f_2d57_4b39_b8e2_6f1a9d3c5e27);

    /// Handle of this DLL (not of the host process).
    pub(crate) fn module() -> HMODULE {
        let mut handle = HMODULE::default();
        // Any address inside this DLL identifies it; use this function's own address.
        let address = module as fn() -> HMODULE as *const u16;
        unsafe {
            let _ = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                windows::core::PCWSTR(address),
                &mut handle,
            );
        }
        handle
    }

    pub(crate) fn dll_path() -> PathBuf {
        let mut buf = vec![0u16; 32768];
        let len = unsafe { GetModuleFileNameW(Some(module()), &mut buf) } as usize;
        PathBuf::from(String::from_utf16_lossy(&buf[..len]))
    }

    #[implement(IClassFactory)]
    struct ClassFactory {
        clsid: GUID,
    }

    impl IClassFactory_Impl for ClassFactory_Impl {
        fn CreateInstance(
            &self,
            outer: Ref<IUnknown>,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> windows::core::Result<()> {
            if ppv.is_null() {
                return Err(E_POINTER.into());
            }
            unsafe { *ppv = null_mut() };
            if !outer.is_null() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let unknown: IUnknown = if self.clsid == CLSID_PREVIEW_HANDLER {
                PreviewHandler::new().into()
            } else {
                ThumbnailProvider::new().into()
            };
            let hr = unsafe { unknown.query(riid, ppv) };
            crate::log(&format!(
                "CreateInstance {:?} riid {:?} -> {hr:?}",
                self.clsid,
                unsafe { *riid }
            ));
            hr.ok()
        }

        fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
            Ok(())
        }
    }

    fn to_hresult(r: windows::core::Result<()>) -> HRESULT {
        match r {
            Ok(()) => S_OK,
            Err(e) => e.code(),
        }
    }

    #[no_mangle]
    unsafe extern "system" fn DllGetClassObject(
        rclsid: *const GUID,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT {
        if ppv.is_null() || rclsid.is_null() {
            return E_POINTER;
        }
        *ppv = null_mut();
        let clsid = *rclsid;
        crate::log(&format!("DllGetClassObject {clsid:?} riid {:?}", *riid));
        if clsid != CLSID_PREVIEW_HANDLER && clsid != CLSID_THUMBNAIL_PROVIDER {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = ClassFactory { clsid }.into();
        factory.query(riid, ppv)
    }

    /// Stay loaded: WebView2 may still call back into this DLL after the last object is released.
    #[no_mangle]
    extern "system" fn DllCanUnloadNow() -> HRESULT {
        S_FALSE
    }

    #[no_mangle]
    extern "system" fn DllRegisterServer() -> HRESULT {
        to_hresult(registry::register())
    }

    #[no_mangle]
    extern "system" fn DllUnregisterServer() -> HRESULT {
        to_hresult(registry::unregister())
    }
}
