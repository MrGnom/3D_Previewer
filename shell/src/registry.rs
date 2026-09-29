//! Per-user COM + shell registration (HKCU, no elevation needed).

use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR},
        System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW,
            RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE,
            REG_SZ, RRF_RT_REG_SZ,
        },
        UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST},
    },
};

use crate::{dll_path, CLSID_PREVIEW_HANDLER, CLSID_THUMBNAIL_PROVIDER};

/// Every format the Babylon.js viewer can show gets the interactive preview.
pub const PREVIEW_EXTENSIONS: &[&str] = &[
    "stl", "obj", "glb", "gltf", "fbx", "ply", "splat", "spz", "babylon",
];
/// Formats the CPU thumbnail renderer understands.
pub const THUMBNAIL_EXTENSIONS: &[&str] = &["stl", "obj", "glb", "gltf", "ply", "splat"];

const PREVIEW_SHELLEX: &str = "{8895b1c6-b41f-4c1c-a562-0d564250836f}";
const THUMBNAIL_SHELLEX: &str = "{e357fccd-a995-4576-b01f-234630154e96}";
/// prevhost.exe surrogate (64-bit).
const PREVHOST_APPID: &str = "{6d2b5079-2f0b-48dd-ab7f-97cec514d30b}";

const PREVIEW_NAME: &str = "Babylon Viewer 3D Preview";
const THUMBNAIL_NAME: &str = "Babylon Viewer 3D Thumbnails";

enum Value<'a> {
    Str(&'a str),
    Dword(u32),
}

fn check(e: WIN32_ERROR) -> windows::core::Result<()> {
    e.ok()
}

fn set(path: &str, name: Option<&str>, value: Value) -> windows::core::Result<()> {
    unsafe {
        let mut key = HKEY::default();
        check(RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(path),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        ))?;
        let name = name.map(HSTRING::from);
        let name_ptr = name
            .as_ref()
            .map(|n| PCWSTR(n.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let result = match value {
            Value::Str(s) => {
                let wide: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
                let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                RegSetValueExW(key, name_ptr, None, REG_SZ, Some(bytes))
            }
            Value::Dword(d) => {
                RegSetValueExW(key, name_ptr, None, REG_DWORD, Some(&d.to_le_bytes()))
            }
        };
        let _ = RegCloseKey(key);
        check(result)
    }
}

fn get_default(path: &str) -> Option<String> {
    let mut buf = [0u16; 128];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(path),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        )
        .ok()
        .ok()?;
    }
    let chars = (len as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..chars]))
}

fn delete_tree(path: &str) {
    unsafe {
        let r = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(path));
        if r != ERROR_FILE_NOT_FOUND {
            let _ = r.ok();
        }
    }
}

fn clsid_string(clsid: &windows::core::GUID) -> String {
    format!("{{{clsid:?}}}")
}

/// Keys through which Explorer finds a shell extension for `.ext`.
fn shellex_keys(ext: &str, handler: &str) -> [String; 2] {
    [
        format!(r"Software\Classes\.{ext}\shellex\{handler}"),
        format!(r"Software\Classes\SystemFileAssociations\.{ext}\shellex\{handler}"),
    ]
}

fn register_server(clsid: &str, name: &str, dll: &str) -> windows::core::Result<()> {
    let key = format!(r"Software\Classes\CLSID\{clsid}");
    set(&key, None, Value::Str(name))?;
    set(&key, Some("DisplayName"), Value::Str(name))?;
    set(&format!(r"{key}\InprocServer32"), None, Value::Str(dll))?;
    set(
        &format!(r"{key}\InprocServer32"),
        Some("ThreadingModel"),
        Value::Str("Apartment"),
    )
}

pub fn register() -> windows::core::Result<()> {
    let dll = dll_path();
    let dll = dll.to_string_lossy();

    let preview = clsid_string(&CLSID_PREVIEW_HANDLER);
    register_server(&preview, PREVIEW_NAME, &dll)?;
    let key = format!(r"Software\Classes\CLSID\{preview}");
    set(&key, Some("AppID"), Value::Str(PREVHOST_APPID))?;
    // Asks for a medium-integrity prevhost.exe. Explorer ignores this for per-user (HKCU)
    // registrations, so the handler also works at low integrity (see crate::data_dir).
    set(&key, Some("DisableLowILProcessIsolation"), Value::Dword(1))?;
    set(
        r"Software\Microsoft\Windows\CurrentVersion\PreviewHandlers",
        Some(&preview),
        Value::Str(PREVIEW_NAME),
    )?;
    for ext in PREVIEW_EXTENSIONS {
        for k in shellex_keys(ext, PREVIEW_SHELLEX) {
            set(&k, None, Value::Str(&preview))?;
        }
    }

    let thumbnail = clsid_string(&CLSID_THUMBNAIL_PROVIDER);
    register_server(&thumbnail, THUMBNAIL_NAME, &dll)?;
    for ext in THUMBNAIL_EXTENSIONS {
        for k in shellex_keys(ext, THUMBNAIL_SHELLEX) {
            set(&k, None, Value::Str(&thumbnail))?;
        }
    }

    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    Ok(())
}

pub fn unregister() -> windows::core::Result<()> {
    let preview = clsid_string(&CLSID_PREVIEW_HANDLER);
    let thumbnail = clsid_string(&CLSID_THUMBNAIL_PROVIDER);

    // Only remove per-extension hooks that still point at us.
    for (exts, shellex, clsid) in [
        (PREVIEW_EXTENSIONS, PREVIEW_SHELLEX, &preview),
        (THUMBNAIL_EXTENSIONS, THUMBNAIL_SHELLEX, &thumbnail),
    ] {
        for ext in exts {
            for k in shellex_keys(ext, shellex) {
                if get_default(&k).is_some_and(|v| v.eq_ignore_ascii_case(clsid)) {
                    delete_tree(&k);
                }
            }
        }
    }
    unsafe {
        let _ = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(r"Software\Microsoft\Windows\CurrentVersion\PreviewHandlers"),
            &HSTRING::from(preview.as_str()),
        );
    }
    delete_tree(&format!(r"Software\Classes\CLSID\{preview}"));
    delete_tree(&format!(r"Software\Classes\CLSID\{thumbnail}"));

    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    Ok(())
}
