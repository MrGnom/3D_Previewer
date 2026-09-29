//! Explorer preview handler: a child window hosting WebView2, which runs the Babylon.js
//! viewer bundle installed next to this DLL. The viewer and the previewed file are served
//! from a private `http://babylonviewer.localhost/` origin through WebResourceRequested, so
//! glTF side files (.bin, textures) resolve relative to the model's folder.

use std::{
    cell::RefCell,
    ffi::c_void,
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicIsize, Ordering},
        mpsc, Arc, Mutex, Once,
    },
    thread,
    time::{Duration, Instant},
};

use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use webview2_com::{
    take_pwstr, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, Microsoft::Web::WebView2::Win32::*,
    NavigationCompletedEventHandler, WebMessageReceivedEventHandler,
    WebResourceRequestedEventHandler,
};
use windows::{
    core::{implement, w, IUnknown, Interface, Ref, Result, BOOL, GUID, HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{
            COLORREF, E_FAIL, E_INVALIDARG, E_POINTER, HWND, LPARAM, LRESULT, RECT, WPARAM,
        },
        Graphics::Gdi::LOGFONTW,
        System::{
            Com::{CoInitializeEx, CoUninitialize, IStream, COINIT_APARTMENTTHREADED},
            Ole::{IObjectWithSite, IObjectWithSite_Impl, IOleWindow, IOleWindow_Impl},
            Threading::GetCurrentThreadId,
        },
        UI::{
            Input::KeyboardAndMouse::GetFocus,
            Shell::{
                IPreviewHandler, IPreviewHandlerFrame, IPreviewHandlerVisuals,
                IPreviewHandlerVisuals_Impl, IPreviewHandler_Impl,
                PropertiesSystem::{IInitializeWithFile, IInitializeWithFile_Impl},
                SHCreateStreamOnFileEx,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
                IsWindow, PeekMessageW, PostThreadMessageW, RegisterClassW, SetParent,
                SetWindowPos, TranslateMessage, HWND_TOP, MSG, PM_NOREMOVE, SWP_NOACTIVATE,
                SWP_NOZORDER, WM_APP, WM_USER, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN,
                WS_EX_NOPARENTNOTIFY, WS_VISIBLE,
            },
        },
    },
};

use crate::module;

const ORIGIN: &str = "http://babylonviewer.localhost";
const WINDOW_CLASS: PCWSTR = w!("BabylonViewerPreviewHost");

/// WebView2 state. Lives only on the preview's UI thread.
#[derive(Default)]
struct State {
    file: Option<PathBuf>,
    parent: HWND,
    rect: RECT,
    background: Option<COLORREF>,
    host: Option<HWND>,
    env: Option<ICoreWebView2Environment>,
    controller: Option<ICoreWebView2Controller>,
    /// Bumped on unload so late WebView2 callbacks for a previous preview are ignored.
    generation: u32,
}

/// What Explorer has told us so far. prevhost.exe calls the handler from a neutral
/// apartment on MTA threads, so this is shared state behind a mutex.
#[derive(Default)]
struct Front {
    file: Option<PathBuf>,
    parent: isize,
    rect: RECT,
    background: Option<COLORREF>,
    site: Option<IUnknown>,
    /// Host window handle, written by the UI thread once created (0 until then).
    host: Arc<AtomicIsize>,
    ui: Option<UiThread>,
}

#[implement(
    IPreviewHandler,
    IPreviewHandlerVisuals,
    IInitializeWithFile,
    IObjectWithSite,
    IOleWindow
)]
pub struct PreviewHandler {
    front: Mutex<Front>,
}

impl PreviewHandler {
    pub fn new() -> Self {
        Self {
            front: Mutex::default(),
        }
    }

    fn front(&self) -> std::sync::MutexGuard<'_, Front> {
        self.front.lock().unwrap_or_else(|e| e.into_inner())
    }
}

enum Command {
    Start {
        file: PathBuf,
        parent: isize,
        rect: RECT,
        background: Option<COLORREF>,
        host: Arc<AtomicIsize>,
    },
    SetParent(isize, RECT),
    Resize(RECT),
    Focus,
    Stop,
}

/// WebView2 needs a single-threaded apartment with a message loop, which prevhost.exe's
/// calling threads are not. Each preview therefore runs on its own STA thread that owns
/// the host window (a child of Explorer's preview pane) and the WebView2 controller.
struct UiThread {
    tx: mpsc::Sender<Command>,
    thread_id: u32,
    handle: thread::JoinHandle<()>,
}

impl UiThread {
    fn spawn() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (id_tx, id_rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("babylon-preview".into())
            .spawn(move || ui_main(rx, id_tx))
            .map_err(|_| E_FAIL)?;
        let thread_id = id_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| E_FAIL)?;
        Ok(Self {
            tx,
            thread_id,
            handle,
        })
    }

    fn send(&self, command: Command) {
        if self.tx.send(command).is_ok() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_APP, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn stop(self) {
        self.send(Command::Stop);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if self.handle.is_finished() {
            let _ = self.handle.join();
        } else {
            crate::log("preview UI thread did not stop in time");
        }
    }
}

fn ui_main(rx: mpsc::Receiver<Command>, id_tx: mpsc::Sender<u32>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut msg = MSG::default();
        // Make sure this thread has a message queue before anyone posts to it.
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let _ = id_tx.send(GetCurrentThreadId());

        let state = Rc::new(RefCell::new(State::default()));
        'pump: while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.hwnd.is_invalid() && msg.message == WM_APP {
                while let Ok(command) = rx.try_recv() {
                    if !handle_command(&state, command) {
                        break 'pump;
                    }
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        unload(&state);
        CoUninitialize();
    }
}

/// Returns false when the thread should exit.
fn handle_command(state: &Rc<RefCell<State>>, command: Command) -> bool {
    match command {
        Command::Start {
            file,
            parent,
            rect,
            background,
            host: host_out,
        } => {
            {
                let mut s = state.borrow_mut();
                s.file = Some(file);
                s.parent = HWND(parent as *mut c_void);
                s.rect = rect;
                s.background = background;
            }
            match create_host_window(state) {
                Ok(host) => {
                    host_out.store(host.0 as isize, Ordering::SeqCst);
                    if let Err(e) = start_webview(state.clone()) {
                        crate::log(&format!("start_webview failed: {e}"));
                    }
                }
                Err(e) => crate::log(&format!("host window creation failed: {e}")),
            }
            true
        }
        Command::SetParent(parent, rect) => {
            let host = {
                let mut s = state.borrow_mut();
                s.parent = HWND(parent as *mut c_void);
                s.host
            };
            if let Some(host) = host {
                unsafe {
                    let _ = SetParent(host, Some(HWND(parent as *mut c_void)));
                }
            }
            resize(state, rect);
            true
        }
        Command::Resize(rect) => {
            resize(state, rect);
            true
        }
        Command::Focus => {
            if let Some(c) = &state.borrow().controller {
                unsafe {
                    let _ = c.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                }
            }
            true
        }
        Command::Stop => false,
    }
}

fn create_host_window(state: &Rc<RefCell<State>>) -> Result<HWND> {
    let mut s = state.borrow_mut();
    if let Some(host) = s.host {
        return Ok(host);
    }
    register_window_class();
    let r = s.rect;
    let host = unsafe {
        // No WM_PARENTNOTIFY: the parent belongs to another thread (Explorer's), and a
        // synchronous notification to it must never be able to stall this thread.
        CreateWindowExW(
            WS_EX_NOPARENTNOTIFY,
            WINDOW_CLASS,
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
            r.left,
            r.top,
            width(&r),
            height(&r),
            Some(s.parent),
            None,
            Some(module().into()),
            None,
        )?
    };
    s.host = Some(host);
    Ok(host)
}

fn resize(state: &Rc<RefCell<State>>, rect: RECT) {
    let mut s = state.borrow_mut();
    s.rect = rect;
    unsafe {
        if let Some(host) = s.host {
            let _ = SetWindowPos(
                host,
                Some(HWND_TOP),
                rect.left,
                rect.top,
                width(&rect),
                height(&rect),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        if let Some(c) = &s.controller {
            let _ = c.SetBounds(RECT {
                left: 0,
                top: 0,
                right: width(&rect),
                bottom: height(&rect),
            });
        }
    }
}

fn unload(state: &Rc<RefCell<State>>) {
    let mut s = state.borrow_mut();
    s.generation = s.generation.wrapping_add(1);
    if let Some(c) = s.controller.take() {
        unsafe {
            let _ = c.Close();
        }
    }
    s.env = None;
    if let Some(host) = s.host.take() {
        unsafe {
            if IsWindow(Some(host)).as_bool() {
                let _ = DestroyWindow(host);
            }
        }
    }
    s.file = None;
}

unsafe extern "system" fn host_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn register_window_class() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(host_wndproc),
            hInstance: module().into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
    });
}

fn width(r: &RECT) -> i32 {
    (r.right - r.left).max(0)
}
fn height(r: &RECT) -> i32 {
    (r.bottom - r.top).max(0)
}

/// The viewer bundle ships at `<install>/viewer` with the DLL at `<install>/shell/`.
fn viewer_dir() -> Option<PathBuf> {
    let dll = crate::dll_path();
    let dir = dll.parent()?;
    [dir.join("viewer"), dir.parent()?.join("viewer")]
        .into_iter()
        .find(|d| d.join("index.html").is_file())
}

fn user_data_dir() -> PathBuf {
    crate::data_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("BabylonViewer"))
        .join("PreviewWebView2")
}

fn colorref_hex(c: COLORREF) -> String {
    let v = c.0;
    format!(
        "{:02x}{:02x}{:02x}",
        v & 0xff,
        (v >> 8) & 0xff,
        (v >> 16) & 0xff
    )
}

/// Maps `/viewer/...` and `/model/...` request paths to files, refusing anything that
/// would escape the base folder.
fn resolve(uri: &str, viewer: &Path, model_dir: &Path) -> Option<PathBuf> {
    let rest = uri.strip_prefix(ORIGIN)?.trim_start_matches('/');
    let rest = rest.split(['?', '#']).next()?;
    let (base, rel) = if let Some(r) = rest.strip_prefix("viewer/") {
        (viewer, r)
    } else if let Some(r) = rest.strip_prefix("model/") {
        (model_dir, r)
    } else {
        return None;
    };
    let mut path = base.to_path_buf();
    for seg in rel.split('/') {
        let seg = percent_decode_str(seg).decode_utf8().ok()?;
        if seg.is_empty() || seg.contains(['\\', ':']) {
            return None;
        }
        let p = Path::new(seg.as_ref());
        if !matches!(p.components().next(), Some(Component::Normal(_)))
            || p.components().count() != 1
        {
            return None;
        }
        path.push(p);
    }
    path.is_file().then_some(path)
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("wasm") => "application/wasm",
        Some("json" | "gltf" | "babylon") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn serve(
    env: &ICoreWebView2Environment,
    args: &ICoreWebView2WebResourceRequestedEventArgs,
    viewer: &Path,
    model_dir: &Path,
) -> Result<()> {
    let request = unsafe { args.Request()? };
    let mut uri = PWSTR::null();
    unsafe { request.Uri(&mut uri)? };
    let uri = take_pwstr(uri);

    let resolved = resolve(&uri, viewer, model_dir);
    crate::log(&format!("request {uri} -> {resolved:?}"));
    let response = match resolved {
        Some(path) => {
            use windows::Win32::System::Com::{STGM_READ, STGM_SHARE_DENY_NONE};
            let stream: IStream = unsafe {
                SHCreateStreamOnFileEx(
                    &HSTRING::from(path.as_os_str()),
                    (STGM_READ | STGM_SHARE_DENY_NONE).0,
                    0,
                    false,
                    None,
                )?
            };
            let headers = format!(
                "Content-Type: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store",
                content_type(&path)
            );
            unsafe {
                env.CreateWebResourceResponse(&stream, 200, w!("OK"), &HSTRING::from(headers))?
            }
        }
        None => unsafe { env.CreateWebResourceResponse(None, 404, w!("Not Found"), w!(""))? },
    };
    unsafe { args.SetResponse(&response) }
}

fn start_webview(state: Rc<RefCell<State>>) -> Result<()> {
    let (generation, file) = {
        let s = state.borrow();
        (s.generation, s.file.clone().ok_or(E_FAIL)?)
    };
    let viewer = viewer_dir().ok_or(E_FAIL)?;
    let model_dir = file.parent().ok_or(E_INVALIDARG)?.to_path_buf();
    let file_name = file
        .file_name()
        .ok_or(E_INVALIDARG)?
        .to_string_lossy()
        .into_owned();

    crate::log(&format!(
        "start webview for {} (viewer at {})",
        file.display(),
        viewer.display()
    ));
    let options: ICoreWebView2EnvironmentOptions = CoreWebView2EnvironmentOptions::default().into();
    let env_state = state.clone();
    let env_handler =
        CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, env| {
            if let Err(e) = &result {
                crate::log(&format!("environment creation failed: {e}"));
            }
            result?;
            let env = env.ok_or(E_POINTER)?;
            let host = {
                let mut s = env_state.borrow_mut();
                if s.generation != generation {
                    return Ok(());
                }
                s.env = Some(env.clone());
                s.host.ok_or(E_FAIL)?
            };

            let ctl_state = env_state.clone();
            let ctl_env = env.clone();
            let ctl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                move |result, controller| {
                    if let Err(e) = &result {
                        crate::log(&format!("controller creation failed: {e}"));
                    }
                    result?;
                    let controller = controller.ok_or(E_POINTER)?;
                    let (rect, background) = {
                        let s = ctl_state.borrow();
                        if s.generation != generation {
                            unsafe { controller.Close()? };
                            return Ok(());
                        }
                        (s.rect, s.background)
                    };

                    unsafe {
                        if let (Some(bg), Ok(c2)) =
                            (background, controller.cast::<ICoreWebView2Controller2>())
                        {
                            let v = bg.0;
                            c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                                A: 255,
                                R: (v & 0xff) as u8,
                                G: ((v >> 8) & 0xff) as u8,
                                B: ((v >> 16) & 0xff) as u8,
                            })?;
                        }
                        controller.SetBounds(RECT {
                            left: 0,
                            top: 0,
                            right: width(&rect),
                            bottom: height(&rect),
                        })?;
                        controller.SetIsVisible(true)?;

                        let webview = controller.CoreWebView2()?;
                        let settings = webview.Settings()?;
                        settings.SetIsStatusBarEnabled(false)?;
                        settings.SetAreDefaultContextMenusEnabled(false)?;
                        settings.SetIsZoomControlEnabled(false)?;
                        settings.SetAreDevToolsEnabled(cfg!(debug_assertions))?;
                        if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
                            s3.SetAreBrowserAcceleratorKeysEnabled(false)?;
                        }

                        let filter = HSTRING::from(format!("{ORIGIN}/*"));
                        webview.AddWebResourceRequestedFilter(
                            &filter,
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                        )?;
                        let serve_env = ctl_env.clone();
                        let (viewer, model_dir) = (viewer.clone(), model_dir.clone());
                        let mut token = 0i64;
                        webview.add_WebResourceRequested(
                            &WebResourceRequestedEventHandler::create(Box::new(
                                move |_sender, args| {
                                    if let Some(args) = args {
                                        // A failure here only affects one request; the page shows its own error.
                                        if let Err(e) =
                                            serve(&serve_env, &args, &viewer, &model_dir)
                                        {
                                            crate::log(&format!("serve failed: {e}"));
                                        }
                                    }
                                    Ok(())
                                },
                            )),
                            &mut token,
                        )?;

                        let mut url = format!(
                            "{ORIGIN}/viewer/index.html?mode=preview&root=%2Fmodel%2F&file={}",
                            utf8_percent_encode(&file_name, NON_ALPHANUMERIC)
                        );
                        if let Some(bg) = background {
                            url.push_str(&format!("&bg={}", colorref_hex(bg)));
                        }
                        let mut nav_token = 0i64;
                        webview.add_NavigationCompleted(
                            &NavigationCompletedEventHandler::create(Box::new(|_sender, args| {
                                if let Some(args) = args {
                                    let mut ok = BOOL::default();
                                    let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                                    let _ = args.IsSuccess(&mut ok);
                                    let _ = args.WebErrorStatus(&mut status);
                                    crate::log(&format!(
                                        "navigation completed: success={} status={}",
                                        ok.as_bool(),
                                        status.0
                                    ));
                                }
                                Ok(())
                            })),
                            &mut nav_token,
                        )?;
                        let mut msg_token = 0i64;
                        webview.add_WebMessageReceived(
                            &WebMessageReceivedEventHandler::create(Box::new(|_sender, args| {
                                if let Some(args) = args {
                                    let mut message = PWSTR::null();
                                    if args.TryGetWebMessageAsString(&mut message).is_ok() {
                                        crate::log(&format!("viewer: {}", take_pwstr(message)));
                                    }
                                }
                                Ok(())
                            })),
                            &mut msg_token,
                        )?;
                        crate::log(&format!("navigate {url}"));
                        webview.Navigate(&HSTRING::from(url))?;
                    }
                    ctl_state.borrow_mut().controller = Some(controller);
                    Ok(())
                },
            ));
            unsafe { env.CreateCoreWebView2Controller(host, &ctl_handler) }
        }));

    let user_data = HSTRING::from(user_data_dir().as_os_str());
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &user_data, &options, &env_handler)
    }
}

impl IPreviewHandler_Impl for PreviewHandler_Impl {
    fn SetWindow(&self, hwnd: HWND, prc: *const RECT) -> Result<()> {
        if prc.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let rect = unsafe { *prc };
        let mut f = self.front();
        f.parent = hwnd.0 as isize;
        f.rect = rect;
        if let Some(ui) = &f.ui {
            ui.send(Command::SetParent(f.parent, rect));
        }
        Ok(())
    }

    fn SetRect(&self, prc: *const RECT) -> Result<()> {
        if prc.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let rect = unsafe { *prc };
        let mut f = self.front();
        f.rect = rect;
        if let Some(ui) = &f.ui {
            ui.send(Command::Resize(rect));
        }
        Ok(())
    }

    fn DoPreview(&self) -> Result<()> {
        let mut f = self.front();
        let file = f.file.clone().ok_or(E_FAIL)?;
        crate::log(&format!("DoPreview {}", file.display()));
        if let Some(old) = f.ui.take() {
            old.stop();
        }
        // Don't wait for the UI thread: creating a child of Explorer's window involves
        // that window's thread, which may be the very thread blocked in this call.
        f.host = Arc::default();
        let ui = UiThread::spawn()?;
        ui.send(Command::Start {
            file,
            parent: f.parent,
            rect: f.rect,
            background: f.background,
            host: f.host.clone(),
        });
        f.ui = Some(ui);
        Ok(())
    }

    fn Unload(&self) -> Result<()> {
        let ui = {
            let mut f = self.front();
            f.file = None;
            f.host = Arc::default();
            f.ui.take()
        };
        if let Some(ui) = ui {
            ui.stop();
        }
        Ok(())
    }

    fn SetFocus(&self) -> Result<()> {
        if let Some(ui) = &self.front().ui {
            ui.send(Command::Focus);
        }
        Ok(())
    }

    fn QueryFocus(&self) -> Result<HWND> {
        let h = unsafe { GetFocus() };
        if h.is_invalid() {
            Err(E_FAIL.into())
        } else {
            Ok(h)
        }
    }

    fn TranslateAccelerator(&self, pmsg: *const MSG) -> Result<()> {
        let site = self.front().site.clone();
        match site.and_then(|s| s.cast::<IPreviewHandlerFrame>().ok()) {
            Some(frame) => unsafe { frame.TranslateAccelerator(pmsg) },
            None => Err(windows::Win32::Foundation::S_FALSE.into()),
        }
    }
}

impl IPreviewHandlerVisuals_Impl for PreviewHandler_Impl {
    fn SetBackgroundColor(&self, color: COLORREF) -> Result<()> {
        self.front().background = Some(color);
        Ok(())
    }

    fn SetFont(&self, _plf: *const LOGFONTW) -> Result<()> {
        Ok(())
    }

    fn SetTextColor(&self, _color: COLORREF) -> Result<()> {
        Ok(())
    }
}

impl IInitializeWithFile_Impl for PreviewHandler_Impl {
    fn Initialize(&self, pszfilepath: &PCWSTR, _grfmode: u32) -> Result<()> {
        let path = unsafe { pszfilepath.to_string() }.map_err(|_| E_INVALIDARG)?;
        crate::log(&format!("Initialize {path}"));
        self.front().file = Some(PathBuf::from(path));
        Ok(())
    }
}

impl IObjectWithSite_Impl for PreviewHandler_Impl {
    fn SetSite(&self, site: Ref<IUnknown>) -> Result<()> {
        self.front().site = site.cloned();
        Ok(())
    }

    fn GetSite(&self, riid: *const GUID, ppvsite: *mut *mut c_void) -> Result<()> {
        if ppvsite.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppvsite = std::ptr::null_mut() };
        match &self.front().site {
            Some(site) => unsafe { site.query(riid, ppvsite).ok() },
            None => Err(E_FAIL.into()),
        }
    }
}

impl IOleWindow_Impl for PreviewHandler_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        let f = self.front();
        let host = f.host.load(Ordering::SeqCst);
        let h = if host != 0 { host } else { f.parent };
        Ok(HWND(h as *mut c_void))
    }

    fn ContextSensitiveHelp(&self, _fentermode: BOOL) -> Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
}
