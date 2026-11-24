#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("Kbrowser requires Windows WebView2.");

mod data;
mod live;
mod store;

use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tauri::{
    State, AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, PhysicalPosition, PhysicalSize,
    Position, Size, Theme, Url, WebviewUrl, Window, WindowEvent,
    webview::{NewWindowResponse, PageLoadEvent, WebviewBuilder},
    window::{Effect, EffectsBuilder, WindowBuilder},
};

/// Height (logical px) of the UI webview: 40 tab strip + 44 toolbar.
const CHROME_H: f64 = 84.0;

struct Ext(Option<PathBuf>);

/// Tracks whether the tab WebView2 environment has already received `extensions_path`.
struct ExtInit(AtomicBool);

/// IDs of the extensions loaded into the running WebView2 profile.
struct Loaded(std::sync::Mutex<Vec<String>>);

/// Whether the Mica backdrop could be applied (Windows 11 only).
struct Mica(bool);

/// Extra height (logical px) the UI webview grows by while the address-bar suggestions are open.
/// Page webviews slide down by the same amount so the list is not covered by them.
struct Suggest(std::sync::Mutex<f64>);

/// Currently active tab webview label.
struct ActiveTab(std::sync::Mutex<Option<String>>);

/// URL passed on the command line at first launch; the UI takes it once at startup.
struct Launch(std::sync::Mutex<Option<String>>);

// --- Session (open tabs + window bounds) ----------------------------------------

#[derive(Clone, Serialize, Deserialize, Default)]
struct WinState {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct Session {
    window: Option<WinState>,
    tabs: Vec<String>,
    active: usize,
}

struct SessionState(std::sync::Mutex<Session>);

#[derive(Clone, Serialize)]
struct TabUpdate {
    id: String,
    url: Option<String>,
    title: Option<String>,
    loading: Option<bool>,
}

fn label(id: &str) -> String {
    format!("tab-{id}")
}

/// Tab webviews share one WebView2 environment, so extension options are passed on first tab creation.
fn configure(
    b: WebviewBuilder<tauri::Wry>,
    ext: Option<&PathBuf>,
) -> WebviewBuilder<tauri::Wry> {
    let b = b.browser_extensions_enabled(true);

    match ext {
        Some(path) => b.extensions_path(path),
        None => b,
    }
}

fn is_localhost_input(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower == "localhost" || lower.starts_with("localhost:") || lower.starts_with("localhost/")
}

fn is_valid_bare_host_url(candidate: &str) -> Option<Url> {
    let u = Url::parse(candidate).ok()?;
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    let host = u.host_str()?;
    if host.is_empty()
        || host == "tauri.localhost"
        || host.starts_with('.')
        || host.ends_with('.')
        || host.contains("..")
    {
        return None;
    }
    Some(u)
}

fn allow_tab_navigation(u: &Url, allowed_internal_path: Option<&str>) -> bool {
    if u.scheme() == "chrome-search" {
        return false;
    }
    if u.host_str() == Some("tauri.localhost") {
        return allowed_internal_path.is_some_and(|p| u.path() == p);
    }
    true
}

fn allow_popup_url(u: &Url) -> bool {
    matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| h != "tauri.localhost")
}

fn allow_open_page_url(raw: &str) -> bool {
    if matches!(raw, "about:extensions" | "about:settings" | "about:new") {
        return true;
    }
    Url::parse(raw)
        .ok()
        .is_some_and(|u| allow_popup_url(&u))
}

fn is_ui_caller(wv: &tauri::Webview) -> bool {
    wv.label() == "ui"
        && wv
            .url()
            .ok()
            .is_some_and(|u| u.host_str() == Some("tauri.localhost"))
}

fn is_internal_page(wv: &tauri::Webview, allowed_paths: &[&str]) -> bool {
    wv.label().starts_with("tab-")
        && wv.url().ok().is_some_and(|u| {
            u.host_str() == Some("tauri.localhost") && allowed_paths.contains(&u.path())
        })
}

fn require_ui(wv: &tauri::Webview) -> Result<(), String> {
    if is_ui_caller(wv) {
        Ok(())
    } else {
        Err("Unauthorized".into())
    }
}

fn require_internal(wv: &tauri::Webview, allowed_paths: &[&str]) -> Result<(), String> {
    if is_internal_page(wv, allowed_paths) {
        Ok(())
    } else {
        Err("Unauthorized".into())
    }
}

//parses different url types
fn normalize(input: &str, data: &data::Data) -> String {
    let s = input.trim();
    if s.eq_ignore_ascii_case("about:blank") {
        return "about:blank".to_string();
    }
    if let Ok(u) = Url::parse(s)
        && matches!(u.scheme(), "http" | "https" | "file" | "chrome-extension")
        && u.host_str() != Some("tauri.localhost")
    {
        return s.to_string();
    }
    if !s.contains(char::is_whitespace) {
        if is_localhost_input(s) {
            let cand = format!("http://{s}");
            if is_valid_bare_host_url(&cand).is_some() {
                return cand;
            }
        }
        if s.contains('.') {
            let cand = format!("https://{s}");
            if is_valid_bare_host_url(&cand).is_some() {
                return cand;
            }
        }
    }
    data.search_url(s)
}

/// Finds the first usable argument (URL or file path) and returns it as a URL.
/// args[0] is the executable itself and is skipped.
fn url_from_args(args: &[String], cwd: Option<&Path>) -> Option<String> {
    for a in args.iter().skip(1) {
        let a = a.trim().trim_matches('"');
        if a.is_empty() || a.starts_with('-') {
            continue;
        }
        if let Ok(u) = Url::parse(a)
            && matches!(u.scheme(), "http" | "https" | "file" | "chrome-extension")
        {
            if u.host_str() != Some("tauri.localhost") {
                return Some(u.to_string());
            }
            continue;
        }
        // Not a URL: treat it as a file path ("C:\x\a.html" parses as scheme "c", so it lands here).
        let mut p = PathBuf::from(a);
        if p.is_relative()
            && let Some(c) = cwd
        {
            p = c.join(p);
        }
        if let Ok(full) = p.canonicalize() {
            let s = full.to_string_lossy();
            let clean = PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s));
            if let Ok(u) = Url::from_file_path(&clean) {
                return Some(u.to_string());
            }
        }
        if !a.contains(char::is_whitespace) {
            if is_localhost_input(a)
                && let Some(u) = is_valid_bare_host_url(&format!("http://{a}"))
            {
                return Some(u.to_string());
            }
            if a.contains('.')
                && let Some(u) = is_valid_bare_host_url(&format!("https://{a}"))
            {
                return Some(u.to_string());
            }
        }
    }
    None
}

fn apply_theme(window: &Window, theme: &str) {
    let t = match theme {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None, // follow the system
    };
    let _ = window.set_theme(t);
}

fn layout(window: &Window) {
    let (Ok(scale), Ok(px)) = (window.scale_factor(), window.inner_size()) else {
        return;
    };
    let size = px.to_logical::<f64>(scale);
    let content_h = (size.height - CHROME_H).max(0.0);
    let extra = *window.app_handle().state::<Suggest>().0.lock().unwrap();
    let active_label = window
        .app_handle()
        .try_state::<ActiveTab>()
        .and_then(|a| a.0.lock().unwrap().clone());

    for (name, wv) in window.app_handle().webviews() {
        if name == "ui" {
            let _ = wv.set_size(Size::Logical(LogicalSize::new(
                size.width,
                CHROME_H + extra,
            )));
        } else if name.starts_with("tab-") {
            // Only update position/size for the visible active tab to prevent background GPU/DWM thrashing
            if active_label.as_ref().is_none_or(|act| act == &name) {
                let _ = wv.set_position(Position::Logical(LogicalPosition::new(
                    0.0,
                    CHROME_H + extra,
                )));
                let _ = wv.set_size(Size::Logical(LogicalSize::new(size.width, content_h)));
            }
        }
    }
}

fn show_only(app: &AppHandle, id: &str, focus_content: bool) {
    let keep = label(id);
    if let Some(act) = app.try_state::<ActiveTab>() {
        *act.0.lock().unwrap() = Some(keep.clone());
    }
    // Update newly visible tab dimensions immediately before showing
    if let Some(w) = app.get_window("main")
        && let (Ok(scale), Ok(px)) = (w.scale_factor(), w.inner_size())
    {
        let size = px.to_logical::<f64>(scale);
        let content_h = (size.height - CHROME_H).max(0.0);
        let extra = *app.state::<Suggest>().0.lock().unwrap();

        for (name, wv) in app.webviews() {
            if name.starts_with("tab-") {
                if name == keep {
                    let _ = wv.set_position(Position::Logical(LogicalPosition::new(
                        0.0,
                        CHROME_H + extra,
                    )));
                    let _ = wv.set_size(Size::Logical(LogicalSize::new(size.width, content_h)));
                    let _ = wv.show();
                    if focus_content {
                        let _ = wv.set_focus();
                    }
                } else {
                    let _ = wv.hide();
                }
            }
        }
        return;
    }

    for (name, wv) in app.webviews() {
        if name.starts_with("tab-") {
            if name == keep {
                let _ = wv.show();
                if focus_content {
                    let _ = wv.set_focus();
                }
            } else {
                let _ = wv.hide();
            }
        }
    }
}

fn emit_tab(app: &AppHandle, update: TabUpdate) {
    let _ = app.emit("tab-updated", update);
}

// --- Session helpers --------------------------------------------------------------

fn session_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("session.json"))
}

fn read_session(app: &AppHandle) -> Session {
    session_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Writes the in-memory session to disk asynchronously in a background thread.
fn persist(app: &AppHandle) {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Unique temp names: two saves in the same millisecond must not share a
    // `.tmp` file (concurrent writes to one path could corrupt the rename).
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let s = app.state::<SessionState>().0.lock().unwrap().clone();
    let Some(path) = session_path(app) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(&s) {
        let dir = path.parent().map(|p| p.to_path_buf());
        std::thread::spawn(move || {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = if let Some(d) = dir {
                d.join(format!("session.{}.{seq}.tmp", data::now()))
            } else {
                path.with_extension("json.tmp")
            };
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        });
    }
}

/// Synchronous write on app shutdown to ensure session data is fully flushed.
fn persist_sync(app: &AppHandle) {
    let s = app.state::<SessionState>().0.lock().unwrap().clone();
    let Some(path) = session_path(app) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(&s) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Remembers the window bounds in memory (the disk write happens on exit).
fn track_window(window: &Window) {
    // A minimized window reports bogus coordinates.
    if window.is_minimized().unwrap_or(false) {
        return;
    }
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.inner_size()) else {
        return;
    };
    let maximized = window.is_maximized().unwrap_or(false);
    let st = window.app_handle().state::<SessionState>();
    let mut s = st.0.lock().unwrap();
    let ws = s.window.get_or_insert_with(Default::default);
    ws.maximized = maximized;
    // While maximized, keep the last normal bounds so "restore" goes back to them.
    if !maximized {
        ws.x = pos.x;
        ws.y = pos.y;
        ws.width = size.width;
        ws.height = size.height;
    }
}

/// True if the window's top edge is still reachable on one of the connected monitors.
fn on_screen(window: &Window, ws: &WinState) -> bool {
    window
        .available_monitors()
        .map(|ms| {
            ms.iter().any(|m| {
                let (p, s) = (m.position(), m.size());
                let cx = ws.x + (ws.width as i32) / 2;
                cx >= p.x
                    && cx < p.x + s.width as i32
                    && ws.y >= p.y
                    && ws.y < p.y + s.height as i32 - 50
            })
        })
        .unwrap_or(false)
}

fn restore_window(window: &Window, ws: &WinState) {
    if ws.width > 0 && ws.height > 0 {
        let _ = window.set_size(PhysicalSize::new(ws.width, ws.height));
        if on_screen(window, ws) {
            let _ = window.set_position(PhysicalPosition::new(ws.x, ws.y));
        }
    }
    if ws.maximized {
        let _ = window.maximize();
    }
}

///loads a session from persistent storage.
#[tauri::command]
fn load_session(webview: tauri::Webview, app: AppHandle) -> Result<Session, String> {
    require_ui(&webview)?;
    Ok(app.state::<SessionState>().0.lock().unwrap().clone())
}

///Deletes the session from persistent storage to prevent unnecessary data retention.
#[tauri::command]
fn delete_session(
    webview: tauri::Webview,
    app: AppHandle,
    state: State<'_, SessionState>,
) -> Result<(), String> {
    require_internal(&webview, &["/settings.html"])?;
    //Lock and completely reset the in-memory runtime session state
    {
        let mut session_lock = state.0.lock().map_err(|e| e.to_string())?;
        *session_lock = Session::default();
    }

    //Overwrite the physical session.json file with the empty baseline data.
    //Must be synchronous: an async write could land after the remove_file below
    //and resurrect the file this command is supposed to delete.
    persist_sync(&app);

    //Optional hard-deletion: Clean up the session file completely from the disk
    if let Some(path) = session_path(&app)
        && path.exists()
    {
        let _ = std::fs::remove_file(path);
    }

    Ok(())
}

///saves the open tabs into persistent storage.
#[tauri::command]
fn save_tabs(
    webview: tauri::Webview,
    app: AppHandle,
    tabs: Vec<String>,
    active: usize,
) -> Result<(), String> {
    require_ui(&webview)?;
    {
        let st = app.state::<SessionState>();
        let mut s = st.0.lock().unwrap();
        s.tabs = tabs;
        s.active = active;
    }
    persist(&app);
    Ok(())
}

///takes the apps launch argument
#[tauri::command]
fn take_launch_url(webview: tauri::Webview, app: AppHandle) -> Result<Option<String>, String> {
    require_ui(&webview)?;
    Ok(app.state::<Launch>().0.lock().unwrap().take())
}

/// creates a new tab. This function handles about: urls seperately, to parse them to their actual
///local path. for extension settings, it prevents their redirection to chrome exclusive urls.
#[tauri::command]
async fn new_tab(
    webview: tauri::Webview,
    app: AppHandle,
    id: String,
    url: String,
) -> Result<(), String> {
    require_ui(&webview)?;
    let window = app.get_window("main").ok_or("main window missing")?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let size = window
        .inner_size()
        .map_err(|e| e.to_string())?
        .to_logical::<f64>(scale);
    let allowed_internal_path: Option<&'static str> = match url.as_str() {
        "about:extensions" => Some("/extensions.html"),
        "about:settings" => Some("/settings.html"),
        "about:new" => Some("/new.html"),
        _ => None,
    };
    let target = match url.as_str() {
        "about:extensions" => WebviewUrl::App("extensions.html".into()),
        "about:settings" => WebviewUrl::App("settings.html".into()),
        "about:new" => WebviewUrl::App("new.html".into()),
        s if s.eq_ignore_ascii_case("about:blank") => {
            WebviewUrl::External("about:blank".parse::<Url>().map_err(|e| e.to_string())?)
        }
        _ => {
            let data = app.state::<data::Data>();
            WebviewUrl::External(
                normalize(&url, &data)
                    .parse::<Url>()
                    .map_err(|e| e.to_string())?,
            )
        }
    };

    let (a1, i1) = (app.clone(), id.clone());
    let (a2, i2) = (app.clone(), id.clone());
    let (a3, i3) = (app.clone(), id.clone());
    let a4 = app.clone();
    let popup_times = std::sync::Arc::new(Mutex::new(VecDeque::<Instant>::new()));

    let builder = WebviewBuilder::new(label(&id), target)
        .on_navigation(move |u| {
            if !allow_tab_navigation(u, allowed_internal_path) {
                return false;
            }
            emit_tab(
                &a1,
                TabUpdate {
                    id: i1.clone(),
                    url: Some(u.to_string()),
                    title: None,
                    loading: None,
                },
            );
            true
        })
        .on_page_load(move |_, p| {
            emit_tab(
                &a2,
                TabUpdate {
                    id: i2.clone(),
                    url: Some(p.url().to_string()),
                    title: None,
                    loading: Some(matches!(p.event(), PageLoadEvent::Started)),
                },
            );
        })
        .on_document_title_changed(move |_, title| {
            emit_tab(
                &a3,
                TabUpdate {
                    id: i3.clone(),
                    url: None,
                    title: Some(title),
                    loading: None,
                },
            );
        })
        // target=_blank and window.open become new tabs, filtered and rate-limited.
        .on_new_window(move |u, _| {
            if allow_popup_url(&u) {
                let now = Instant::now();
                let mut times = popup_times.lock().unwrap();
                while times
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1))
                {
                    times.pop_front();
                }
                if times.len() < 3 {
                    times.push_back(now);
                    let _ = a4.emit("open-tab", u.to_string());
                }
            }
            NewWindowResponse::Deny
        });

    let first_ext = !app.state::<ExtInit>().0.swap(true, Ordering::AcqRel);
    let ext_path = if first_ext {
        app.state::<Ext>().0.clone()
    } else {
        None
    };

    window
        .add_child(
            configure(builder, ext_path.as_ref()),
            LogicalPosition::new(0.0, CHROME_H),
            LogicalSize::new(size.width, (size.height - CHROME_H).max(0.0)),
        )
        .map_err(|e| e.to_string())?;
    show_only(&app, &id, true);
    Ok(())
}

///shows the given tabs contents.
#[tauri::command]
async fn activate_tab(
    webview: tauri::Webview,
    app: AppHandle,
    id: String,
    focus: Option<bool>,
) -> Result<(), String> {
    require_ui(&webview)?;
    show_only(&app, &id, focus.unwrap_or(true));
    Ok(())
}

///closes a tab.
#[tauri::command]
fn close_tab(webview: tauri::Webview, app: AppHandle, id: String) -> Result<(), String> {
    require_ui(&webview)?;
    if let Some(wv) = app.get_webview(&label(&id)) {
        wv.close()
            .map_err(|e| format!("Failed to close tab {id}: {e}"))?;
    }

    Ok(())
}

///trigers the closing of the app. This happens for example, when the last tab is closed.
#[tauri::command]
fn exit_app(webview: tauri::Webview, app: tauri::AppHandle) -> Result<(), String> {
    require_ui(&webview)?;
    persist_sync(&app);
    app.exit(0);
    Ok(())
}

///this is the navigation bar.
#[tauri::command]
async fn navigate(
    webview: tauri::Webview,
    app: AppHandle,
    id: String,
    input: String,
) -> Result<(), String> {
    require_ui(&webview)?;
    let wv = app.get_webview(&label(&id)).ok_or("no such tab")?;
    let url = {
        let data = app.state::<data::Data>();
        normalize(&input, &data)
            .parse::<Url>()
            .map_err(|e| e.to_string())?
    };
    wv.navigate(url).map_err(|e| e.to_string())
}

/// Navigates the calling tab webview (used by internal pages such as `about:new`),
/// delegating to `normalize` so search encoding and local/extension schemes match the address bar.
#[tauri::command]
async fn navigate_self(
    webview: tauri::Webview,
    app: AppHandle,
    input: String,
) -> Result<(), String> {
    require_internal(&webview, &["/new.html"])?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if matches!(trimmed, "about:extensions" | "about:settings" | "about:new") {
        let _ = app.emit("open-tab", trimmed.to_string());
        return Ok(());
    }
    let url = {
        let data = app.state::<data::Data>();
        normalize(trimmed, &data)
            .parse::<Url>()
            .map_err(|e| e.to_string())?
    };
    webview.navigate(url).map_err(|e| e.to_string())
}

/// Forwards keyboard chrome actions from internal pages (`about:new`) to the `ui` webview.
#[tauri::command]
async fn ui_action(
    webview: tauri::Webview,
    app: AppHandle,
    action: String,
) -> Result<(), String> {
    require_internal(&webview, &["/new.html"])?;
    if !matches!(action.as_str(), "new-tab" | "focus-addr" | "open-settings") {
        return Ok(());
    }
    if action == "focus-addr"
        && let Some(ui) = app.get_webview("ui")
    {
        let _ = ui.set_focus();
    }
    let _ = app.emit("ui-action", action);
    Ok(())
}

///this is the navbar
#[tauri::command]
async fn nav(
    webview: tauri::Webview,
    app: AppHandle,
    id: String,
    action: String,
) -> Result<(), String> {
    require_ui(&webview)?;
    let wv = app.get_webview(&label(&id)).ok_or("no such tab")?;
    let js = match action.as_str() {
        "back" => "history.back()",
        "forward" => "history.forward()",
        "reload" => "location.reload()",
        _ => return Err("unknown action".into()),
    };
    wv.eval(js).map_err(|e| e.to_string())
}

///minimizes the current window.
#[tauri::command]
async fn win_minimize(webview: tauri::Webview, window: Window) -> Result<(), String> {
    require_ui(&webview)?;
    let _ = window.minimize();
    Ok(())
}

#[tauri::command]
async fn win_toggle_maximize(webview: tauri::Webview, window: Window) -> Result<(), String> {
    require_ui(&webview)?;
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
    Ok(())
}

///returns if the current window is maximized.
#[tauri::command]
async fn win_is_maximized(webview: tauri::Webview, window: Window) -> Result<bool, String> {
    require_ui(&webview)?;
    Ok(window.is_maximized().unwrap_or(false))
}

///closes the window.
#[tauri::command]
async fn win_close(webview: tauri::Webview, window: Window) -> Result<(), String> {
    require_ui(&webview)?;
    let _ = window.close();
    Ok(())
}

///queries wether the system has "mica", on Windows 11 this checks if transparency is enabled.
#[tauri::command]
fn has_mica(webview: tauri::Webview, app: AppHandle) -> Result<bool, String> {
    require_ui(&webview)?;
    Ok(app.state::<Mica>().0)
}

// --- Settings -------------------------------------------------------------------

///provides settings from the saved settings in settings.json
#[tauri::command]
fn get_settings(webview: tauri::Webview, app: AppHandle) -> Result<data::Settings, String> {
    if !is_ui_caller(&webview)
        && !is_internal_page(&webview, &["/new.html", "/settings.html", "/extensions.html"])
    {
        return Err("Unauthorized".into());
    }
    Ok(app.state::<data::Data>().settings())
}

///sets new setting/s
#[tauri::command]
async fn set_settings(
    webview: tauri::Webview,
    app: AppHandle,
    settings: data::Settings,
) -> Result<(), String> {
    require_internal(&webview, &["/settings.html"])?;
    let saved = app.state::<data::Data>().set_settings(settings);
    if let Some(w) = app.get_window("main") {
        apply_theme(&w, &saved.theme);
    }
    app.emit("settings-changed", saved)
        .map_err(|e| e.to_string())
}

///provides the webview version for display in html.
#[tauri::command]
fn about_webview(webview: tauri::Webview) -> Result<String, String> {
    require_internal(&webview, &["/settings.html"])?;
    Ok(tauri::webview_version().unwrap_or_else(|_| "unknown".into()))
}

/// Opens an internal page or a web address in the browser's tab strip.
#[tauri::command]
fn open_page(webview: tauri::Webview, app: AppHandle, url: String) -> Result<(), String> {
    require_internal(&webview, &["/new.html", "/settings.html"])?;
    if !allow_open_page_url(&url) {
        return Err("Invalid URL".into());
    }
    let _ = app.emit("open-tab", url);
    Ok(())
}

// --- History --------------------------------------------------------------------

///adds an entry to the browsing history.
#[tauri::command]
fn history_add(
    webview: tauri::Webview,
    app: AppHandle,
    url: String,
    title: String,
) -> Result<(), String> {
    require_ui(&webview)?;
    if app.state::<data::Data>().add_history(&url, &title) {
        let _ = app.emit("history-updated", ());
    }
    Ok(())
}

#[tauri::command]
fn history_suggest(
    webview: tauri::Webview,
    app: AppHandle,
    query: String,
) -> Result<Vec<data::Suggestion>, String> {
    require_ui(&webview)?;
    Ok(app.state::<data::Data>().suggest(&query, 6))
}

/// Grows (or shrinks back) the UI webview for the address-bar suggestion list.
#[tauri::command]
async fn set_suggest_height(
    webview: tauri::Webview,
    app: AppHandle,
    height: f64,
) -> Result<(), String> {
    require_ui(&webview)?;
    let height = height.clamp(0.0, 400.0);
    {
        let suggest_state = app.state::<Suggest>();
        let mut cur = suggest_state.0.lock().unwrap();
        if (*cur - height).abs() < 1.0 {
            return Ok(()); // Height hasn't changed; avoid redundant DWM layout recalculations
        }
        *cur = height;
    }
    if let Some(w) = app.get_window("main") {
        layout(&w);
    }
    Ok(())
}

///provides a list of all entries in the browsing history for display in html.
#[tauri::command]
fn history_list(
    webview: tauri::Webview,
    app: AppHandle,
    query: String,
) -> Result<Vec<data::Entry>, String> {
    require_internal(&webview, &["/settings.html", "/new.html"])?;
    Ok(app.state::<data::Data>().history(&query))
}

///removes history entry
#[tauri::command]
fn history_remove(
    webview: tauri::Webview,
    app: AppHandle,
    time: u64,
    url: String,
) -> Result<(), String> {
    require_internal(&webview, &["/settings.html"])?;
    app.state::<data::Data>().remove(time, &url);
    let _ = app.emit("history-updated", ());
    Ok(())
}

///deletes history within given range.
#[tauri::command]
fn history_clear(
    webview: tauri::Webview,
    app: AppHandle,
    range: String,
) -> Result<(), String> {
    require_internal(&webview, &["/settings.html"])?;
    app.state::<data::Data>().clear(&range);
    let _ = app.emit("history-updated", ());
    Ok(())
}

// --- Extensions -----------------------------------------------------------------

///hands the given extension to store.rs for installation handling
#[tauri::command]
async fn install_extension(
    webview: tauri::Webview,
    app: AppHandle,
    input: String,
) -> Result<store::ExtInfo, String> {
    if !is_ui_caller(&webview) && !is_internal_page(&webview, &["/extensions.html"]) {
        return Err("Unauthorized".into());
    }
    store::install(&app, &input).await
}

///provides a list of installed extensions for display in html.
#[tauri::command]
async fn list_extensions(
    webview: tauri::Webview,
    app: AppHandle,
) -> Result<Vec<store::ExtInfo>, String> {
    require_internal(&webview, &["/extensions.html", "/settings.html"])?;
    Ok(store::list(&app))
}

///removes extension file.
#[tauri::command]
async fn remove_extension(
    webview: tauri::Webview,
    app: AppHandle,
    id: String,
) -> Result<(), String> {
    require_internal(&webview, &["/extensions.html"])?;
    store::remove(&app, &id).await
}

/// Opens the extension's options page (chrome-extension://<id>/<page>) in a new tab.
#[tauri::command]
async fn open_extension_options(
    webview: tauri::Webview,
    app: AppHandle,
    id: String,
) -> Result<(), String> {
    require_internal(&webview, &["/extensions.html", "/settings.html"])?;
    let url = store::options_url(&app, &id).await?;
    app.emit("open-tab", url).map_err(|e| e.to_string())
}

#[tauri::command]
fn restart_app(webview: tauri::Webview, app: AppHandle) -> Result<(), String> {
    require_internal(&webview, &["/extensions.html"])?;
    persist_sync(&app);
    app.restart();
}

/// this builds the app. the invoke handler gets handed all functions we need to be able to use in our frontend/javascript.
fn main() {
    tauri::Builder::default()
        // captures new cli process launches as tabs in this instance
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            if let Some(url) = url_from_args(&args, Some(Path::new(&cwd))) {
                let _ = app.emit("open-tab", url);
            }
            if let Some(w) = app.get_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .invoke_handler(tauri::generate_handler![
            new_tab,
            activate_tab,
            close_tab,
            exit_app,
            navigate,
            navigate_self,
            ui_action,
            nav,
            install_extension,
            list_extensions,
            remove_extension,
            open_extension_options,
            restart_app,
            get_settings,
            set_settings,
            about_webview,
            open_page,
            history_add,
            history_list,
            history_suggest,
            history_remove,
            history_clear,
            set_suggest_height,
            take_launch_url,
            load_session,
            delete_session,
            save_tabs,
            has_mica,
            win_minimize,
            win_toggle_maximize,
            win_is_maximized,
            win_close
        ])
        .setup(|app| {
            // unpacked Chrome extensions go in <app data dir>/extensions
            let base = app.path().app_data_dir().ok();
            let ui_profile = base.as_ref().map(|d| d.join("ui-profile"));
            if let Some(dir) = &ui_profile {
                let _ = std::fs::create_dir_all(dir);
            }
            let ext = base.as_ref().map(|d| d.join("extensions"));
            if let Some(dir) = &ext {
                let _ = std::fs::create_dir_all(dir);
            }
            let loaded = ext.as_deref().map(store::prepare).unwrap_or_default();
            let data = data::Data::load(base);
            let theme = data.settings().theme;
            app.manage(Ext(ext.clone()));
            app.manage(ExtInit(AtomicBool::new(false)));
            app.manage(Loaded(std::sync::Mutex::new(loaded)));
            app.manage(Suggest(std::sync::Mutex::new(0.0)));
            app.manage(ActiveTab(std::sync::Mutex::new(None)));
            app.manage(data);

            // url given in command line
            let launch = url_from_args(&std::env::args().collect::<Vec<_>>(), None);
            app.manage(Launch(std::sync::Mutex::new(launch)));

            // Last session: open tabs and window bounds.
            let session = read_session(app.handle());
            let restore = session.window.clone();
            app.manage(SessionState(std::sync::Mutex::new(session)));

            let window = WindowBuilder::new(app, "main")
                .title("Kbrowser")
                .inner_size(1200.0, 800.0)
                .min_inner_size(640.0, 400.0)
                // Hidden until position/size are restored, so it doesn't flash at the default spot.
                .visible(false)
                // Custom frame: the tab strip is the title bar. With shadow(true), Windows keeps
                // native resize handles outside the client area and rounds corners on Windows 11.
                .decorations(false)
                // Transparent so the Mica backdrop can show through the (transparent) UI webview.
                .transparent(true)
                .shadow(true)
                .build()?;

            // Mica needs Windows 11; on Windows 10 this fails and the UI keeps a solid strip.
            let mica = window
                .set_effects(EffectsBuilder::new().effect(Effect::Mica).build())
                .is_ok();
            app.manage(Mica(mica));
            apply_theme(&window, &theme);

            // Restore before reading the size below, so the UI webview gets the right width.
            if let Some(ws) = &restore {
                restore_window(&window, ws);
            }

            let size = window
                .inner_size()?
                .to_logical::<f64>(window.scale_factor()?);
            let mut ui_builder = WebviewBuilder::new("ui", WebviewUrl::App("index.html".into()))
                .transparent(true)
                .browser_extensions_enabled(false);
            if let Some(dir) = ui_profile {
                ui_builder = ui_builder.data_directory(dir);
            }
            window.add_child(
                ui_builder,
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(size.width, CHROME_H),
            )?;

            window.show()?;

            // Window movement and bounds captured without blocking the UI drag loop
            let w = window.clone();
            window.on_window_event(move |e| match e {
                WindowEvent::Resized(_) => {
                    layout(&w);
                    track_window(&w);
                }
                WindowEvent::CloseRequested { .. } => {
                    track_window(&w);
                    persist_sync(w.app_handle());
                }
                _ => {}
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Kbrowser");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize() {
        let data = data::Data::load(None);
        assert_eq!(normalize("https://example.com", &data), "https://example.com");
        assert_eq!(normalize("http://example.com/foo", &data), "http://example.com/foo");
        assert_eq!(normalize("about:blank", &data), "about:blank");
        assert_eq!(normalize("ABOUT:BLANK", &data), "about:blank");
        assert_eq!(normalize("localhost", &data), "http://localhost");
        assert_eq!(normalize("localhost:3000", &data), "http://localhost:3000");
        assert_eq!(normalize("localhost/foo", &data), "http://localhost/foo");
        assert_eq!(normalize("localhost.com", &data), "https://localhost.com");
        assert_eq!(normalize("github.com", &data), "https://github.com");
        assert_eq!(normalize("what is rust", &data), "https://duckduckgo.com/?q=what+is+rust");
        assert_eq!(normalize("...", &data), "https://duckduckgo.com/?q=...");
        assert_eq!(normalize(".env", &data), "https://duckduckgo.com/?q=.env");
        assert_eq!(normalize("foo..bar", &data), "https://duckduckgo.com/?q=foo..bar");
    }

    #[test]
    fn test_url_from_args() {
        let args = vec![
            "kbrowser.exe".into(),
            "https://tauri.app".into(),
        ];
        assert_eq!(url_from_args(&args, None), Some("https://tauri.app/".into()));

        let args_flag = vec![
            "kbrowser.exe".into(),
            "--devtools".into(),
            "https://crates.io".into(),
        ];
        assert_eq!(url_from_args(&args_flag, None), Some("https://crates.io/".into()));

        let args_bare = vec!["kbrowser.exe".into(), "example.com".into()];
        assert_eq!(
            url_from_args(&args_bare, None),
            Some("https://example.com/".into())
        );

        let args_local = vec!["kbrowser.exe".into(), "localhost:3000".into()];
        assert_eq!(
            url_from_args(&args_local, None),
            Some("http://localhost:3000/".into())
        );

        let args_bad = vec![
            "kbrowser.exe".into(),
            "http://tauri.localhost/index.html".into(),
            "javascript:alert(1)".into(),
        ];
        assert_eq!(url_from_args(&args_bad, None), None);
    }

    #[test]
    fn test_navigation_and_popup_policy() {
        let chrome_search = Url::parse("chrome-search://local-ntp/").unwrap();
        assert!(!allow_tab_navigation(&chrome_search, None));

        let ui_url = Url::parse("http://tauri.localhost/index.html").unwrap();
        assert!(!allow_tab_navigation(&ui_url, None));
        assert!(!allow_tab_navigation(&ui_url, Some("/settings.html")));

        let settings_url = Url::parse("http://tauri.localhost/settings.html").unwrap();
        assert!(!allow_tab_navigation(&settings_url, None));
        assert!(!allow_tab_navigation(&settings_url, Some("/new.html")));
        assert!(allow_tab_navigation(&settings_url, Some("/settings.html")));

        let https_url = Url::parse("https://example.com/").unwrap();
        assert!(allow_tab_navigation(&https_url, None));
        assert!(allow_popup_url(&https_url));

        assert!(!allow_popup_url(&ui_url));
        assert!(!allow_popup_url(&settings_url));
        assert!(!allow_popup_url(&Url::parse("file:///C:/Windows/win.ini").unwrap()));
        assert!(!allow_popup_url(&Url::parse("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/opt.html").unwrap()));
        assert!(!allow_popup_url(&Url::parse("about:extensions").unwrap()));
        assert!(!allow_popup_url(&Url::parse("javascript:alert(1)").unwrap()));

        assert!(allow_open_page_url("about:settings"));
        assert!(allow_open_page_url("about:extensions"));
        assert!(allow_open_page_url("about:new"));
        assert!(allow_open_page_url("https://chromewebstore.google.com/"));
        assert!(!allow_open_page_url("about:blank"));
        assert!(!allow_open_page_url("http://tauri.localhost/index.html"));
        assert!(!allow_open_page_url("file:///C:/Windows/win.ini"));
    }
}

