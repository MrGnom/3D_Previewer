#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    borrow::Cow,
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Serialize;
use tauri::{http, AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

/// Extensions the viewer can load (keep in sync with viewer/src/viewer.ts).
const EXTENSIONS: &[&str] = &[
    "stl", "obj", "glb", "gltf", "fbx", "ply", "splat", "spz", "sog", "babylon", "usdz", "usd",
    "usda", "usdc", "bvh", "step", "stp", "iges", "igs", "brep",
];

/// Models (and their side files: .bin, textures, .mtl) are served to the webview through
/// this custom protocol. On Windows, `model://` is exposed as `http://model.localhost/`.
const MODEL_SCHEME: &str = "model";
const MODEL_ORIGIN: &str = "http://model.localhost";

/// Characters escaped in each path segment of a model URL.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

#[derive(Default)]
struct AppState {
    /// Folders the webview may read from: only those of models the user opened.
    allowed_dirs: Mutex<HashSet<PathBuf>>,
    initial: Mutex<Option<PathBuf>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelRef {
    root_url: String,
    file_name: String,
    display_name: String,
}

fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

/// `C:\Models\a b` -> `C%3A/Models/a%20b`, `\\server\share\x` -> `UNC/server/share/x`.
fn encode_dir(dir: &Path) -> String {
    let s = dir.to_string_lossy().replace('\\', "/");
    let (prefix, rest) = match s.strip_prefix("//") {
        Some(unc) => ("UNC/", unc.to_string()),
        None => ("", s),
    };
    let encoded: Vec<String> = rest
        .split('/')
        .filter(|p| !p.is_empty())
        .map(encode_segment)
        .collect();
    format!("{prefix}{}", encoded.join("/"))
}

fn decode_url_path(url_path: &str) -> Option<PathBuf> {
    let decoded: Vec<String> = url_path
        .trim_start_matches('/')
        .split('/')
        .map(|p| percent_decode_str(p).decode_utf8().map(|c| c.into_owned()))
        .collect::<Result<_, _>>()
        .ok()?;
    let joined = if decoded.first().map(String::as_str) == Some("UNC") {
        format!(r"\\{}", decoded[1..].join("\\"))
    } else {
        decoded.join("\\")
    };
    let path = PathBuf::from(joined);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    Some(path)
}

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn model_ref(state: &AppState, path: &Path) -> Result<ModelRef, String> {
    let path = std::path::absolute(path).map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err(format!("File not found: {}", path.display()));
    }
    if !is_supported(&path) {
        return Err(format!("Unsupported file type: {}", path.display()));
    }
    let dir = path
        .parent()
        .ok_or("File has no parent folder")?
        .to_path_buf();
    let name = path
        .file_name()
        .ok_or("Invalid file name")?
        .to_string_lossy()
        .into_owned();
    let root_url = format!("{MODEL_ORIGIN}/{}/", encode_dir(&dir));
    state.allowed_dirs.lock().unwrap().insert(dir);
    Ok(ModelRef {
        root_url,
        file_name: encode_segment(&name),
        display_name: name,
    })
}

#[tauri::command]
fn initial_model(state: State<'_, AppState>) -> Result<Option<ModelRef>, String> {
    let initial = state.initial.lock().unwrap().take();
    initial.map(|p| model_ref(&state, &p)).transpose()
}

#[tauri::command]
fn open_path(state: State<'_, AppState>, path: String) -> Result<Option<ModelRef>, String> {
    model_ref(&state, Path::new(&path)).map(Some)
}

#[tauri::command]
async fn pick_model(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<ModelRef>, String> {
    let picked = app
        .dialog()
        .file()
        .set_title("Open 3D model")
        .add_filter("3D models", EXTENSIONS)
        .add_filter("All files", &["*"])
        .blocking_pick_file();
    match picked {
        Some(file) => {
            let path = file.into_path().map_err(|e| e.to_string())?;
            model_ref(&state, &path).map(Some)
        }
        None => Ok(None),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("gltf") => "model/gltf+json",
        Some("glb") => "model/gltf-binary",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ktx2") => "image/ktx2",
        Some("json" | "babylon") => "application/json",
        Some("obj" | "mtl" | "usda" | "bvh") => "text/plain",
        _ => "application/octet-stream",
    }
}

fn respond(status: u16, mime: &str, body: Vec<u8>) -> http::Response<Cow<'static, [u8]>> {
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, mime)
        .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Cow::Owned(body))
        .unwrap()
}

fn serve_model_file(state: &AppState, url_path: &str) -> http::Response<Cow<'static, [u8]>> {
    let Some(path) = decode_url_path(url_path) else {
        return respond(400, "text/plain", b"bad path".to_vec());
    };
    let allowed = state
        .allowed_dirs
        .lock()
        .unwrap()
        .iter()
        .any(|d| path.starts_with(d));
    if !allowed {
        return respond(403, "text/plain", b"forbidden".to_vec());
    }
    match std::fs::read(&path) {
        Ok(bytes) => respond(200, content_type(&path), bytes),
        Err(_) => respond(404, "text/plain", b"not found".to_vec()),
    }
}

fn main() {
    let initial = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .find(|p| !p.to_string_lossy().starts_with('-'));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            initial: Mutex::new(initial),
            ..Default::default()
        })
        .register_uri_scheme_protocol(MODEL_SCHEME, |ctx, request| {
            let state = ctx.app_handle().state::<AppState>();
            serve_model_file(&state, request.uri().path())
        })
        .invoke_handler(tauri::generate_handler![
            initial_model,
            open_path,
            pick_model
        ])
        .run(tauri::generate_context!())
        .expect("error while running Babylon Viewer");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_path_round_trip() {
        for p in [r"C:\Models\my part #1", r"\\server\share\3d\ü"] {
            let dir = PathBuf::from(p);
            let url = format!("/{}/file.stl", encode_dir(&dir));
            assert_eq!(decode_url_path(&url).unwrap(), dir.join("file.stl"));
        }
    }

    #[test]
    fn rejects_parent_dir() {
        assert!(decode_url_path("/C%3A/Models/../secret.txt").is_none());
    }
}
