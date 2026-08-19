mod ipc;
mod widgets;

use ipc::{IpcClient, IpcEvent};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder};
use tauri::path::BaseDirectory;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use widgets::WidgetManifest;

const TRAY_ID: &str = "main-tray";
const MAIN_WINDOW: &str = "main";
const WIDGET_WINDOW_PREFIX: &str = "widget-";
const WIDGETS_RESOURCE_DIR: &str = "resources/widgets";
const WIDGETS_DATA_DIR: &str = "widgets";
const WAKE_CHIME_MARKER_FILE: &str = ".wake_chime_bootstrapped";
const WAKE_CHIME_RESOURCE: &str = "resources/wake_chime.wav";
const LOCAL_SETTINGS_FILE: &str = "local_settings.json";

/// Purely local, jarvis-app-only settings -- never round-trip to the daemon.
/// Distinct from Project-JARVIS's own settings (confirmation mode, wake
/// chime, providers), which live on the daemon and are shared across every
/// connected client.
#[derive(Serialize, serde::Deserialize, Default)]
struct LocalSettings {
    /// Project-JARVIS#146 / jarvisos-app#17: self-quit when the daemon
    /// broadcasts DAEMON_SHUTDOWN. Off by default -- the daemon shutting
    /// down doesn't have to mean this app does too.
    #[serde(default)]
    quit_on_daemon_shutdown: bool,
    /// jarvisos-app#18: widget ids the user has turned on. A widget with no
    /// entry here is disabled -- absent, not false, is the default state so
    /// a freshly-installed widget doesn't appear enabled out of nowhere.
    #[serde(default)]
    enabled_widgets: Vec<String>,
    /// Last known on-screen position per widget id, in physical pixels --
    /// restored the next time that widget's window is created so a
    /// dragged-and-repositioned widget doesn't snap back to the default
    /// corner on every restart.
    #[serde(default)]
    widget_positions: HashMap<String, (i32, i32)>,
    /// Widget id -> local directory overriding its bundled/installed
    /// appearance (a user-supplied HTML/CSS/JS bundle, same shape as any
    /// other widget directory). Absent means "use the widget's own files".
    #[serde(default)]
    widget_appearance_overrides: HashMap<String, String>,
}

fn local_settings_path(app: &AppHandle) -> Option<PathBuf> {
    Some(app.path().app_data_dir().ok()?.join(LOCAL_SETTINGS_FILE))
}

fn read_local_settings(app: &AppHandle) -> LocalSettings {
    local_settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_local_settings(app: &AppHandle, settings: &LocalSettings) -> std::io::Result<()> {
    let path = local_settings_path(app)
        .ok_or_else(|| std::io::Error::other("could not resolve app data dir"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json =
        serde_json::to_string_pretty(settings).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(path, json)
}

/// Resolves (bundled chime path, marker path) if the one-time
/// WAKE_CHIME_PATH override (Project-JARVIS#139's "proof of modularity"
/// criterion) hasn't happened yet. None if it already has, or if the
/// bundled resource/app data dir can't be resolved.
fn pending_wake_chime_bootstrap(app: &AppHandle) -> Option<(String, PathBuf)> {
    let marker_path = app.path().app_data_dir().ok()?.join(WAKE_CHIME_MARKER_FILE);
    let resource_path = app
        .path()
        .resolve(WAKE_CHIME_RESOURCE, BaseDirectory::Resource)
        .ok()?;
    wake_chime_bootstrap_decision(marker_path, resource_path)
}

/// Pure decision logic split out from `pending_wake_chime_bootstrap` so it's
/// testable without a running Tauri app / AppHandle.
fn wake_chime_bootstrap_decision(
    marker_path: PathBuf,
    resource_path: PathBuf,
) -> Option<(String, PathBuf)> {
    if marker_path.exists() {
        return None;
    }
    if !resource_path.is_file() {
        return None;
    }
    Some((resource_path.to_string_lossy().into_owned(), marker_path))
}

// ---------------------------------------------------------------------------
// Widget plugin system (jarvisos-app#18)
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct WidgetInfo {
    #[serde(flatten)]
    manifest: WidgetManifest,
    enabled: bool,
}

fn bundled_widgets_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .resolve(WIDGETS_RESOURCE_DIR, BaseDirectory::Resource)
        .ok()
}

fn installed_widgets_dir(app: &AppHandle) -> Option<PathBuf> {
    Some(app.path().app_data_dir().ok()?.join(WIDGETS_DATA_DIR))
}

fn discover_all_widgets(app: &AppHandle) -> Vec<WidgetManifest> {
    widgets::discover_widgets(
        bundled_widgets_dir(app).as_deref(),
        installed_widgets_dir(app).as_deref(),
    )
}

fn find_widget(app: &AppHandle, id: &str) -> Option<WidgetManifest> {
    discover_all_widgets(app).into_iter().find(|w| w.id == id)
}

fn widget_window_label(id: &str) -> String {
    format!("{WIDGET_WINDOW_PREFIX}{id}")
}

/// Resolves which directory should actually serve a widget's files: a
/// user-supplied appearance override if one is set for this id, otherwise
/// the widget's own bundled/installed directory.
fn widget_serve_dir(app: &AppHandle, manifest: &WidgetManifest) -> PathBuf {
    read_local_settings(app)
        .widget_appearance_overrides
        .get(&manifest.id)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| manifest.dir.clone())
}

/// Opens (or, if already open, focuses) the widget's overlay window --
/// borderless, transparent, always-on-top, restored to its last dragged
/// position if one was saved.
fn open_widget_window(app: &AppHandle, manifest: &WidgetManifest) {
    let label = widget_window_label(&manifest.id);
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.show();
        let _ = existing.set_focus();
        return;
    }

    let url = format!("widget://{}/{}", manifest.id, manifest.entry);
    let Ok(parsed) = url.parse() else { return };
    let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(parsed))
        .title(&manifest.name)
        .inner_size(160.0, 160.0)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false);

    if let Some(&(x, y)) = read_local_settings(app).widget_positions.get(&manifest.id) {
        builder = builder.position(x as f64, y as f64);
    }

    let _ = builder.build();
}

fn close_widget_window(app: &AppHandle, id: &str) {
    if let Some(window) = app.get_webview_window(&widget_window_label(id)) {
        let _ = window.close();
    }
}

/// Re-opens every widget the user had enabled last session -- called once
/// at startup, mirroring how the main window itself always starts shown.
fn reopen_enabled_widgets(app: &AppHandle) {
    let enabled = read_local_settings(app).enabled_widgets;
    if enabled.is_empty() {
        return;
    }
    let manifests = discover_all_widgets(app);
    for id in enabled {
        if let Some(manifest) = manifests.iter().find(|w| w.id == id) {
            open_widget_window(app, manifest);
        }
    }
}

fn widget_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Serves a widget's own files under a dedicated `widget://<id>/<path>`
/// scheme (jarvisos-app#18), rather than the app's own bundled asset
/// protocol -- installed widgets live at an arbitrary runtime path in the
/// data dir, unknown at build time, so they can't be part of the app's own
/// `frontendDist`. Registering our own scheme also means every response
/// here gets its own explicit Content-Security-Policy header, satisfying
/// the "each widget window has its own CSP" requirement directly rather
/// than relying on `on_web_resource_request` (which the tauri:// protocol
/// alone supports).
fn widget_protocol_response(
    app: &AppHandle,
    request: &tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::{header, StatusCode};

    let not_found = || {
        tauri::http::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Vec::new())
            .unwrap()
    };

    let uri = request.uri();
    let Some(id) = uri.host() else {
        return not_found();
    };
    let rel_path = uri.path().trim_start_matches('/');
    if rel_path.is_empty() || rel_path.split('/').any(|seg| seg.is_empty() || seg == "..") {
        return not_found();
    }

    let Some(manifest) = find_widget(app, id) else {
        return not_found();
    };
    let serve_dir = widget_serve_dir(app, &manifest);
    let Ok(canonical_dir) = serve_dir.canonicalize() else {
        return not_found();
    };
    let Ok(canonical_file) = serve_dir.join(rel_path).canonicalize() else {
        return not_found();
    };
    // Belt-and-braces against a crafted path escaping the widget's own
    // directory -- the ".." segment check above should already catch this.
    if !canonical_file.starts_with(&canonical_dir) {
        return not_found();
    }

    let Ok(bytes) = std::fs::read(&canonical_file) else {
        return not_found();
    };
    let content_type = widget_content_type(&canonical_file);

    let mut builder = tauri::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type);
    if content_type == "text/html" {
        // No fetch/XHR/WebSocket to anywhere, no inline/remote scripts --
        // a widget only ever needs to render and listen for Tauri events.
        builder = builder.header(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; connect-src 'none'; frame-src 'none'; object-src 'none';",
        );
    }
    builder.body(bytes).unwrap_or_else(|_| not_found())
}

struct AppState {
    ipc: Arc<Mutex<IpcClient>>,
    is_listening: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    daemon_state: Arc<Mutex<String>>,
}

#[derive(Serialize, Clone)]
struct ChunkPayload {
    content: String,
    done: bool,
}

#[derive(Serialize, Clone)]
struct ConfirmPayload {
    id: String,
    tool_names: Vec<String>,
}

#[derive(Serialize, Clone)]
struct StatusPayload {
    connected: bool,
    state: String,
}

#[derive(Serialize, Clone)]
struct SessionSwitchedPayload {
    session: serde_json::Value,
    messages: Vec<serde_json::Value>,
}

#[derive(Serialize, Clone)]
struct SettingsPayload {
    confirmation_mode: String,
    wake_chime_path: String,
}

#[derive(Serialize, Clone)]
struct ConfigUpdatedPayload {
    key: String,
    value: String,
}

#[derive(Serialize, Clone)]
struct ConfigErrorPayload {
    key: String,
    message: String,
}

#[derive(Serialize, Clone)]
struct DaemonShutdownPayload {
    state: String,
    goals: Vec<serde_json::Value>,
    session_id: Option<String>,
    timestamp: f64,
}

#[tauri::command]
fn send_message(content: String, state: State<AppState>) {
    if content.trim().is_empty() {
        return;
    }
    state.ipc.lock().unwrap().send_message(content);
}

#[tauri::command]
fn toggle_listening(state: State<AppState>, app: AppHandle) {
    let was_listening = state.is_listening.load(Ordering::Relaxed);
    if was_listening {
        state.ipc.lock().unwrap().stop_listening();
        state.is_listening.store(false, Ordering::Relaxed);
        let _ = app.emit("ipc-state", "idle");
    } else {
        state.ipc.lock().unwrap().start_listening();
        state.is_listening.store(true, Ordering::Relaxed);
        let _ = app.emit("ipc-state", "listening");
    }
}

#[tauri::command]
fn stop_stream(state: State<AppState>) {
    state.ipc.lock().unwrap().stop_stream();
}

#[tauri::command]
fn send_confirmation_response(id: String, approved: bool, state: State<AppState>) {
    state
        .ipc
        .lock()
        .unwrap()
        .confirmation_response(id, approved);
}

#[tauri::command]
fn list_confirmations(state: State<AppState>) {
    state.ipc.lock().unwrap().list_confirmations();
}

#[tauri::command]
fn approve_confirmation(id: String, state: State<AppState>) {
    state.ipc.lock().unwrap().approve_confirmation(id);
}

#[tauri::command]
fn deny_confirmation(id: String, state: State<AppState>) {
    state.ipc.lock().unwrap().deny_confirmation(id);
}

/// Approve only the listed items of a batch; the daemon denies the rest (#187).
#[tauri::command]
fn partial_approve_confirmation(id: String, approved_indices: Vec<usize>, state: State<AppState>) {
    state
        .ipc
        .lock()
        .unwrap()
        .partial_approve_confirmation(id, approved_indices);
}

#[tauri::command]
fn approve_all_confirmations(state: State<AppState>) {
    state.ipc.lock().unwrap().approve_all_confirmations();
}

#[tauri::command]
fn list_sessions(state: State<AppState>) {
    state.ipc.lock().unwrap().list_sessions();
}

#[tauri::command]
fn create_session(title: Option<String>, state: State<AppState>) {
    state.ipc.lock().unwrap().create_session(title);
}

#[tauri::command]
fn switch_session(id: String, state: State<AppState>) {
    state.ipc.lock().unwrap().switch_session(id);
}

#[tauri::command]
fn rename_session(id: String, title: String, state: State<AppState>) {
    state.ipc.lock().unwrap().rename_session(id, title);
}

#[tauri::command]
fn delete_session(id: String, state: State<AppState>) {
    state.ipc.lock().unwrap().delete_session(id);
}

#[tauri::command]
fn get_settings(state: State<AppState>) {
    state.ipc.lock().unwrap().get_settings();
}

// Read-only troubleshooting info for the settings panel's General tab --
// the socket path itself, not a live setting, so it doesn't round-trip
// through the daemon.
#[tauri::command]
fn get_connection_info() -> String {
    ipc::socket_path()
}

#[tauri::command]
fn set_confirmation_mode(mode: String, state: State<AppState>) {
    state.ipc.lock().unwrap().set_confirmation_mode(mode);
}

#[tauri::command]
fn reset_wake_chime_path(state: State<AppState>) {
    state.ipc.lock().unwrap().reset_wake_chime_path();
}

#[tauri::command]
fn list_providers(state: State<AppState>) {
    state.ipc.lock().unwrap().list_providers();
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn add_provider(
    ptype: String,
    model: String,
    name: Option<String>,
    url: Option<String>,
    api_key: Option<String>,
    temperature: Option<f64>,
    state: State<AppState>,
) {
    state
        .ipc
        .lock()
        .unwrap()
        .add_provider(ptype, model, name, url, api_key, temperature);
}

#[tauri::command]
fn edit_provider(name: String, fields: serde_json::Value, state: State<AppState>) {
    state.ipc.lock().unwrap().edit_provider(name, fields);
}

#[tauri::command]
fn remove_provider(name: String, state: State<AppState>) {
    state.ipc.lock().unwrap().remove_provider(name);
}

#[tauri::command]
async fn pick_wake_chime_file(app: AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .add_filter("WAV audio", &["wav"])
        .pick_file(move |picked| {
            let _ = tx.send(picked);
        });
    rx.recv().ok().flatten().map(|p| p.to_string())
}

// Project-JARVIS#146 / jarvisos-app#17: connected-clients query + graceful
// shutdown. list_clients must be called (and its result shown to the user)
// before request_daemon_shutdown is ever sent -- the frontend enforces the
// ordering, these are just the two IPC round-trips it needs.
#[tauri::command]
fn list_clients(state: State<AppState>) {
    state.ipc.lock().unwrap().list_clients();
}

#[tauri::command]
fn request_daemon_shutdown(state: State<AppState>) {
    state.ipc.lock().unwrap().request_daemon_shutdown();
}

#[tauri::command]
fn get_quit_on_daemon_shutdown(app: AppHandle) -> bool {
    read_local_settings(&app).quit_on_daemon_shutdown
}

#[tauri::command]
fn set_quit_on_daemon_shutdown(enabled: bool, app: AppHandle) {
    let mut settings = read_local_settings(&app);
    settings.quit_on_daemon_shutdown = enabled;
    if let Err(e) = write_local_settings(&app, &settings) {
        eprintln!("jarvis-ui: failed to write local settings: {e}");
    }
}

#[tauri::command]
fn list_widgets(app: AppHandle) -> Vec<WidgetInfo> {
    let enabled = read_local_settings(&app).enabled_widgets;
    discover_all_widgets(&app)
        .into_iter()
        .map(|manifest| {
            let is_enabled = enabled.contains(&manifest.id);
            WidgetInfo {
                manifest,
                enabled: is_enabled,
            }
        })
        .collect()
}

#[tauri::command]
fn set_widget_enabled(id: String, enabled: bool, app: AppHandle) {
    let mut settings = read_local_settings(&app);
    settings.enabled_widgets.retain(|w| w != &id);
    if enabled {
        settings.enabled_widgets.push(id.clone());
    }
    if let Err(e) = write_local_settings(&app, &settings) {
        eprintln!("jarvis-ui: failed to write local settings: {e}");
        return;
    }

    if enabled {
        if let Some(manifest) = find_widget(&app, &id) {
            open_widget_window(&app, &manifest);
        }
    } else {
        close_widget_window(&app, &id);
    }
}

#[tauri::command]
async fn pick_widget_appearance_folder(id: String, app: AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    let picked = rx.recv().ok().flatten()?;
    let path = picked.into_path().ok()?;
    if !path.join("manifest.json").is_file() {
        // Not a widget directory (no manifest.json) -- reject rather than
        // silently pointing the widget at a folder that can't ever load.
        return None;
    }
    let path_str = path.to_string_lossy().into_owned();

    let mut settings = read_local_settings(&app);
    settings
        .widget_appearance_overrides
        .insert(id.clone(), path_str.clone());
    if let Err(e) = write_local_settings(&app, &settings) {
        eprintln!("jarvis-ui: failed to write local settings: {e}");
        return None;
    }
    // Reopen so the change is visible immediately if the widget is active.
    if settings.enabled_widgets.contains(&id) {
        close_widget_window(&app, &id);
        if let Some(manifest) = find_widget(&app, &id) {
            open_widget_window(&app, &manifest);
        }
    }
    Some(path_str)
}

#[tauri::command]
fn reset_widget_appearance(id: String, app: AppHandle) {
    let mut settings = read_local_settings(&app);
    settings.widget_appearance_overrides.remove(&id);
    if let Err(e) = write_local_settings(&app, &settings) {
        eprintln!("jarvis-ui: failed to write local settings: {e}");
        return;
    }
    if settings.enabled_widgets.contains(&id) {
        close_widget_window(&app, &id);
        if let Some(manifest) = find_widget(&app, &id) {
            open_widget_window(&app, &manifest);
        }
    }
}

// Frontend pulls this once on load to reconcile state in case ipc-connected /
// ipc-state fired (from the backend's IPC poll thread, started in setup())
// before the webview finished loading and registering its event listeners --
// Tauri does not queue events for listeners that attach after emission.
#[tauri::command]
fn get_status(state: State<AppState>) -> StatusPayload {
    StatusPayload {
        connected: state.connected.load(Ordering::Relaxed),
        state: state.daemon_state.lock().unwrap().clone(),
    }
}

fn set_tray_tooltip(app: &AppHandle, tooltip: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(tooltip));
    }
}

fn toggle_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    let _ = window.show();
    let _ = window.set_focus();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let ipc = Arc::new(Mutex::new(IpcClient::new()));
    let ipc_poll = ipc.clone();
    let is_listening = Arc::new(AtomicBool::new(false));
    let is_listening_poll = is_listening.clone();
    let connected = Arc::new(AtomicBool::new(false));
    let connected_poll = connected.clone();
    let daemon_state = Arc::new(Mutex::new("offline".to_string()));
    let daemon_state_poll = daemon_state.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .register_uri_scheme_protocol("widget", |ctx, request| {
            widget_protocol_response(ctx.app_handle(), &request)
        })
        .manage(AppState {
            ipc,
            is_listening,
            connected,
            daemon_state,
        })
        .setup(|app| {
            let handle = app.handle().clone();
            let wake_chime_bootstrap = pending_wake_chime_bootstrap(&handle);
            reopen_enabled_widgets(&handle);
            std::thread::Builder::new()
                .name("jarvis-ipc-poll".into())
                .spawn(move || {
                    poll_loop(
                        ipc_poll,
                        is_listening_poll,
                        connected_poll,
                        daemon_state_poll,
                        handle,
                        wake_chime_bootstrap,
                    )
                })
                .expect("failed to spawn IPC poll thread");

            let show_hide = MenuItemBuilder::with_id("show_hide", "Show/Hide JARVIS").build(app)?;
            let autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);
            let start_on_login = CheckMenuItemBuilder::with_id("start_on_login", "Start on Login")
                .checked(autostart_enabled)
                .build(app)?;
            let shutdown_daemon =
                MenuItemBuilder::with_id("shutdown_daemon", "Shut Down JARVIS...").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let tray_menu = MenuBuilder::new(app)
                .item(&show_hide)
                .item(&start_on_login)
                .separator()
                .item(&shutdown_daemon)
                .item(&quit)
                .build()?;

            let mut tray_builder = TrayIconBuilder::with_id(TRAY_ID)
                .menu(&tray_menu)
                .tooltip("JARVIS -- Offline")
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show_hide" => toggle_main_window(app),
                    "start_on_login" => {
                        let mgr = app.autolaunch();
                        let enabled = mgr.is_enabled().unwrap_or(false);
                        let _ = if enabled { mgr.disable() } else { mgr.enable() };
                    }
                    "shutdown_daemon" => {
                        // Just surfaces the same confirmation modal the settings
                        // panel uses -- the tray can't show a client list or
                        // collect confirmation itself, and shouldn't duplicate
                        // that UI (Project-JARVIS#146 / jarvisos-app#17).
                        show_main_window(app);
                        let _ = app.emit("ipc-open-shutdown-modal", ());
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_main_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray_builder = tray_builder.icon(icon);
            }
            tray_builder.build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == MAIN_WINDOW {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
                return;
            }

            if let Some(id) = window.label().strip_prefix(WIDGET_WINDOW_PREFIX) {
                // Persisted so a dragged widget restores to where the user
                // left it, not the default corner, on the next launch.
                if let WindowEvent::Moved(position) = event {
                    let app = window.app_handle();
                    let mut settings = read_local_settings(app);
                    settings
                        .widget_positions
                        .insert(id.to_string(), (position.x, position.y));
                    let _ = write_local_settings(app, &settings);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            toggle_listening,
            stop_stream,
            send_confirmation_response,
            get_status,
            list_confirmations,
            approve_confirmation,
            deny_confirmation,
            partial_approve_confirmation,
            approve_all_confirmations,
            list_sessions,
            create_session,
            switch_session,
            rename_session,
            delete_session,
            get_settings,
            get_connection_info,
            set_confirmation_mode,
            reset_wake_chime_path,
            list_providers,
            add_provider,
            edit_provider,
            remove_provider,
            pick_wake_chime_file,
            list_clients,
            request_daemon_shutdown,
            get_quit_on_daemon_shutdown,
            set_quit_on_daemon_shutdown,
            list_widgets,
            set_widget_enabled,
            pick_widget_appearance_folder,
            reset_widget_appearance,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}

fn poll_loop(
    ipc: Arc<Mutex<IpcClient>>,
    is_listening: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    daemon_state: Arc<Mutex<String>>,
    app: AppHandle,
    mut wake_chime_bootstrap: Option<(String, PathBuf)>,
) {
    let set_state = |s: &str| *daemon_state.lock().unwrap() = s.to_string();

    loop {
        loop {
            let event = ipc.lock().unwrap().try_recv();
            let Some(ev) = event else { break };
            match ev {
                IpcEvent::Connected => {
                    connected.store(true, Ordering::Relaxed);
                    set_state("idle");
                    set_tray_tooltip(&app, "JARVIS -- Connected");
                    let _ = app.emit("ipc-connected", ());
                    let _ = app.emit("ipc-state", "idle");
                    // Retried on every (re)connect until config_updated
                    // confirms it -- see the ConfigUpdated/ConfigError arms.
                    if let Some((path, _)) = &wake_chime_bootstrap {
                        ipc.lock().unwrap().set_wake_chime_path(path.clone());
                    }
                }
                IpcEvent::Disconnected => {
                    connected.store(false, Ordering::Relaxed);
                    is_listening.store(false, Ordering::Relaxed);
                    set_state("offline");
                    set_tray_tooltip(&app, "JARVIS -- Offline");
                    let _ = app.emit("ipc-disconnected", ());
                    let _ = app.emit("ipc-state", "offline");
                }
                IpcEvent::State(s) => {
                    let listening = s == "listening";
                    is_listening.store(listening, Ordering::Relaxed);
                    set_state(&s);
                    let _ = app.emit("ipc-state", s);
                }
                IpcEvent::ResponseChunk { content, done } => {
                    let _ = app.emit("ipc-chunk", ChunkPayload { content, done });
                    if done {
                        set_state("idle");
                        let _ = app.emit("ipc-state", "idle");
                    }
                }
                IpcEvent::WakeWordDetected => {
                    is_listening.store(true, Ordering::Relaxed);
                    set_state("listening");
                    let _ = app.emit("ipc-wake", ());
                    let _ = app.emit("ipc-state", "listening");
                }
                IpcEvent::ConfirmationRequest { id, tool_names } => {
                    let _ = app.emit("ipc-confirm", ConfirmPayload { id, tool_names });
                }
                IpcEvent::ConfirmationList(list) => {
                    let _ = app.emit("ipc-confirmation-list", list);
                }
                IpcEvent::Ack => {}
                IpcEvent::ConfigUpdated { key, value } => {
                    if key == "WAKE_CHIME_PATH" {
                        if let Some((_, marker_path)) = wake_chime_bootstrap.take() {
                            if let Err(e) = std::fs::write(&marker_path, b"1") {
                                eprintln!("jarvis-ui: failed to write wake chime marker: {e}");
                            }
                        }
                    }
                    // Beyond the one-time bootstrap side effect above, every
                    // config_updated is also forwarded to the frontend so the
                    // settings panel (jarvisos-app#12) can reflect writes it
                    // didn't itself just make (e.g. another connected client
                    // changing CONFIRMATION_MODE).
                    let _ = app.emit("ipc-config-updated", ConfigUpdatedPayload { key, value });
                }
                IpcEvent::ConfigError { key, message } => {
                    if key == "WAKE_CHIME_PATH" && wake_chime_bootstrap.is_some() {
                        eprintln!(
                            "jarvis-ui: daemon rejected default wake chime override: {message}"
                        );
                    }
                    let _ = app.emit("ipc-config-error", ConfigErrorPayload { key, message });
                }
                IpcEvent::Error(message) => {
                    // Was silently discarded, so a daemon-side error or a
                    // malformed line left the UI showing nothing at all --
                    // its siblings (SessionError/ConfigError) both surface.
                    let _ = app.emit("ipc-error", message);
                }
                IpcEvent::SessionList(sessions) => {
                    let _ = app.emit("ipc-session-list", sessions);
                }
                IpcEvent::SessionSwitched { session, messages } => {
                    let _ = app.emit(
                        "ipc-session-switched",
                        SessionSwitchedPayload { session, messages },
                    );
                }
                IpcEvent::SessionError(message) => {
                    let _ = app.emit("ipc-session-error", message);
                }
                IpcEvent::Settings {
                    confirmation_mode,
                    wake_chime_path,
                } => {
                    let _ = app.emit(
                        "ipc-settings",
                        SettingsPayload {
                            confirmation_mode,
                            wake_chime_path,
                        },
                    );
                }
                IpcEvent::ProviderList(providers) => {
                    let _ = app.emit("ipc-provider-list", providers);
                }
                IpcEvent::ProviderError(message) => {
                    let _ = app.emit("ipc-provider-error", message);
                }
                IpcEvent::ClientList(clients) => {
                    let _ = app.emit("ipc-client-list", clients);
                }
                IpcEvent::DaemonShutdown {
                    state,
                    goals,
                    session_id,
                    timestamp,
                } => {
                    let _ = app.emit(
                        "ipc-daemon-shutdown",
                        DaemonShutdownPayload {
                            state,
                            goals,
                            session_id,
                            timestamp,
                        },
                    );
                    // Optional, user-controlled (jarvisos-app#17) -- off by
                    // default. The daemon is already tearing itself down at
                    // this point, so there's no in-flight IPC to race here.
                    if read_local_settings(&app).quit_on_daemon_shutdown {
                        app.exit(0);
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jarvis-ui-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pending_when_marker_absent_and_resource_present() {
        let dir = temp_dir("pending");
        let marker = dir.join(WAKE_CHIME_MARKER_FILE);
        let resource = dir.join("wake_chime.wav");
        fs::write(&resource, b"fake wav bytes").unwrap();

        let result = wake_chime_bootstrap_decision(marker.clone(), resource.clone());
        assert_eq!(
            result,
            Some((resource.to_string_lossy().into_owned(), marker))
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn none_when_marker_already_written() {
        let dir = temp_dir("already-done");
        let marker = dir.join(WAKE_CHIME_MARKER_FILE);
        let resource = dir.join("wake_chime.wav");
        fs::write(&marker, b"1").unwrap();
        fs::write(&resource, b"fake wav bytes").unwrap();

        assert_eq!(wake_chime_bootstrap_decision(marker, resource), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn none_when_bundled_resource_is_missing() {
        let dir = temp_dir("missing-resource");
        let marker = dir.join(WAKE_CHIME_MARKER_FILE);
        let resource = dir.join("wake_chime.wav"); // never created

        assert_eq!(wake_chime_bootstrap_decision(marker, resource), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
