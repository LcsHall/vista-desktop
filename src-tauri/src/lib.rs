// Vista Platform — Tauri shell.
//
// Architecture: one borderless, maximized window with a custom title bar
// webview on top and N "content" webviews below it — one per open TAB.
// Only the active tab's webview is shown; the rest are hidden (their
// pages stay loaded, so switching is instant and preserves state).
//
//   ┌─────────────────────────────────────────────┐
//   │ ⛵ │ Platform │ POS ✕ │ Messaging ✕ │ – ☐ ✕ │  title bar (40px)
//   ├─────────────────────────────────────────────┤
//   │                                               │
//   │   active tab's content webview fills here     │
//   │                                               │
//   └─────────────────────────────────────────────┘
//
// The title bar lives in this repo (HTML/CSS/JS in /src). The first tab
// ("Platform", label `content`) loads the live production app. Opening
// POS / Messaging / any /manage/* page from the web app spawns a new tab
// instead of a browser window or OS window; genuinely external links
// (the public booking site, the VISTA Consulting handoff to Interface)
// still open in the system browser.
//
// Why multi-webview instead of an iframe? An iframe is subject to
// X-Frame-Options / CSP frame-ancestors and would break Supabase auth
// popups. Each webview is a top-level browsing context — same as a real
// browser tab — so auth, cookies, and websockets all work normally. All
// tabs share the app's single WebView2 profile, so the signed-in session
// cookie is shared across them (no re-login per tab).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
// `Manager` brings get_webview / get_window / state / manage into scope;
// `Emitter` brings emit (title-bar notifications). Both, plus the
// multi-webview API (WindowBuilder / WebviewBuilder / Window::add_child /
// get_webview), are unstable-gated — keep `features=["unstable"]`.
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, PhysicalSize, Url, WebviewBuilder,
    WebviewUrl, WindowBuilder, WindowEvent,
};

// Production URL the first (Platform) tab loads. MUST be the Platform
// host, not app.vistainterface.com.
//
// Login lives on Interface (app.vistainterface.com/login). When an
// unauthenticated user hits platform.vistainterface.com, Platform's
// proxy sets a `vista_postlogin_target` cookie on `.vistainterface.com`
// and *then* redirects to the Interface login. The Interface login reads
// that cookie after a successful sign-in and hands the session back to
// Platform (cross-subdomain handoff). Loading app.vistainterface.com
// directly skips the cookie-setting redirect, so login has no post-login
// target and dumps the user on Interface instead of Platform. The extra
// redirect hop is load-bearing — do not "optimize" it away.
const PRODUCTION_URL: &str = "https://platform.vistainterface.com";

// Host of the Platform app. Used to tell "internal staff page" (open as
// a new tab) apart from "external link" (open in the system browser).
const PLATFORM_HOST: &str = "platform.vistainterface.com";

// Label of the first, non-closeable tab (the live Platform app).
const PLATFORM_TAB: &str = "content";

// Height of the custom title bar in logical pixels.
const TITLE_BAR_HEIGHT: f64 = 40.0;

// Initial restore-down window size (the window opens maximized).
const INITIAL_WIDTH: f64 = 1280.0;
const INITIAL_HEIGHT: f64 = 800.0;

// Monotonic counter for unique tab webview labels.
static TAB_COUNTER: AtomicU32 = AtomicU32::new(0);

// window.close() in a browser only closes a window that was opened by
// script. Tab webviews are opened natively by Rust, so a page's "✕ Close"
// button (which calls window.close()) is ignored by the engine and the
// document just goes blank. We bridge it: an injected script reroutes
// window.close() to a sentinel navigation, which the tab's on_navigation
// handler catches and turns into a real tab close. `.invalid` is a
// reserved TLD (RFC 6761) that never resolves, so it can't hit a server.
const CLOSE_SENTINEL: &str = "https://vista-desktop.invalid/__close__";
const CLOSE_BRIDGE_SCRIPT: &str = r#"(function () {
  window.close = function () {
    window.location.href = 'https://vista-desktop.invalid/__close__';
  };
})();"#;

/// Marks the page as running inside the desktop app. The web app reads
/// `window.__VISTA_DESKTOP__` to suppress its "install the desktop app"
/// prompt. Injected at document start into every content/tab webview.
fn desktop_marker_script() -> String {
    format!("window.__VISTA_DESKTOP__ = '{}';", env!("CARGO_PKG_VERSION"))
}

// Ordered list of open tabs (by webview label) + which one is showing.
// order[0] is always PLATFORM_TAB. Stored as managed Tauri state.
struct TabState {
    order: Vec<String>,
    active: String,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Updater plugin: endpoint + pubkey configured in tauri.conf.json.
        // Registration alone does NOTHING in Tauri 2 — the launch check
        // lives in spawn_update_check() below. (The old `"dialog": true`
        // config was a v1 relic the v2 plugin silently ignored; every
        // 0.1.0-0.1.3 install shipped with a dormant updater because of
        // it and needs one manual reinstall to pick up this fix.)
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let window = WindowBuilder::new(app, "main")
                .title("Vista Platform")
                .inner_size(INITIAL_WIDTH, INITIAL_HEIGHT)
                .min_inner_size(900.0, 600.0)
                // decorations(false) = no native chrome; we draw our own.
                .decorations(false)
                // Open maximized; inner_size is the restore-down size.
                .maximized(true)
                .center()
                .build()?;

            // Title bar webview — loads /src/index.html.
            window.add_child(
                WebviewBuilder::new("title_bar", WebviewUrl::App("index.html".into())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(INITIAL_WIDTH, TITLE_BAR_HEIGHT),
            )?;

            // First tab: the live production app.
            let prod_url: Url = PRODUCTION_URL
                .parse()
                .expect("PRODUCTION_URL must be a valid URL");
            let nw_app = app.handle().clone();
            let content = WebviewBuilder::new(PLATFORM_TAB, WebviewUrl::External(prod_url))
                .initialization_script(desktop_marker_script())
                .on_new_window(move |url, _features| {
                    route_new_window(&nw_app, url);
                    NewWindowResponse::Deny
                });
            window.add_child(
                content,
                LogicalPosition::new(0.0, TITLE_BAR_HEIGHT),
                LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT - TITLE_BAR_HEIGHT),
            )?;

            // Seed tab state with the Platform tab active.
            app.manage(Mutex::new(TabState {
                order: vec![PLATFORM_TAB.to_string()],
                active: PLATFORM_TAB.to_string(),
            }));

            // Keep the title bar + the active tab sized to the window.
            let resize_app = app.handle().clone();
            window.on_window_event(move |event| {
                if let WindowEvent::Resized(_) = event {
                    relayout(&resize_app);
                }
            });
            // Initial layout once the window is on-screen (the OS may
            // adjust the size before the webviews attach).
            relayout(app.handle());

            // Non-blocking startup update check (silent when offline or
            // already current). Without this the updater never runs.
            spawn_update_check(app.handle());

            Ok(())
        })
        // IPC commands invoked from the title bar (see /src/main.js).
        .invoke_handler(tauri::generate_handler![
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_is_maximized,
            switch_tab,
            close_tab,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ─── Auto-update ────────────────────────────────────────────────────────

/// Check the GitHub Releases feed once at startup; on a newer signed build,
/// offer Install/Later. Install downloads, applies, and restarts. Failure
/// paths are deliberately silent — offline launches and 404s (no release
/// yet) must never bother a medspa mid-checkin. Same pattern as the
/// vista-admin cockpit, where it's verified end to end.
fn spawn_update_check(app: &tauri::AppHandle) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    use tauri_plugin_updater::UpdaterExt;

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let Ok(updater) = handle.updater() else { return };
        let Ok(Some(update)) = updater.check().await else { return };
        let version = update.version.clone();
        let restart_handle = handle.clone();
        handle
            .dialog()
            .message(format!(
                "Vista Platform {version} is available. Install and restart now?"
            ))
            .title("Update available")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Install".into(),
                "Later".into(),
            ))
            .show(move |confirmed| {
                if !confirmed {
                    return;
                }
                tauri::async_runtime::spawn(async move {
                    if update.download_and_install(|_, _| {}, || {}).await.is_ok() {
                        restart_handle.restart();
                    }
                });
            });
    });
}

// ─── Tab management ─────────────────────────────────────────────────

/// Decide where a new-window request (target="_blank" / window.open)
/// goes: an internal /manage/* page on the Platform host opens as a new
/// tab; everything else opens in the system browser. Window/webview
/// mutations are deferred to the main thread to avoid reentrancy while
/// the webview engine is mid new-window-request.
fn route_new_window(app: &AppHandle, url: Url) {
    let is_internal_page =
        url.host_str() == Some(PLATFORM_HOST) && url.path().starts_with("/manage/");
    if is_internal_page {
        let app = app.clone();
        let _ = app.clone().run_on_main_thread(move || open_tab(&app, url));
    } else if let Err(e) = open::that_detached(url.as_str()) {
        eprintln!("[vista-desktop] failed to open {url} in browser: {e}");
    }
}

/// Add a new content webview as a tab, make it active, and tell the
/// title bar to render it. Runs on the main thread.
fn open_tab(app: &AppHandle, url: Url) {
    let window = match app.get_window("main") {
        Some(w) => w,
        None => return,
    };
    let label = format!("tab-{}", TAB_COUNTER.fetch_add(1, Ordering::Relaxed));
    let (lw, lh) = logical_inner(&window).unwrap_or((INITIAL_WIDTH, INITIAL_HEIGHT));
    let content_h = (lh - TITLE_BAR_HEIGHT).max(0.0);

    let nw_app = app.clone();
    let nav_app = app.clone();
    let nav_label = label.clone();
    let builder = WebviewBuilder::new(label.as_str(), WebviewUrl::External(url.clone()))
        // Tabs can themselves open further tabs / external links.
        .on_new_window(move |u, _features| {
            route_new_window(&nw_app, u);
            NewWindowResponse::Deny
        })
        // Mark the desktop app, then bridge window.close() → sentinel nav.
        .initialization_script(format!("{}{}", desktop_marker_script(), CLOSE_BRIDGE_SCRIPT))
        .on_navigation(move |nav_url| {
            if nav_url.as_str().starts_with(CLOSE_SENTINEL) {
                let a = nav_app.clone();
                let l = nav_label.clone();
                let _ = nav_app.run_on_main_thread(move || close_tab_impl(&a, &l));
                return false; // cancel the sentinel nav; the tab is closing
            }
            true
        });

    if let Err(e) = window.add_child(
        builder,
        LogicalPosition::new(0.0, TITLE_BAR_HEIGHT),
        LogicalSize::new(lw, content_h),
    ) {
        eprintln!("[vista-desktop] failed to open tab for {url}: {e}");
        return;
    }

    {
        let state = app.state::<Mutex<TabState>>();
        let mut s = state.lock().unwrap();
        s.order.push(label.clone());
    }
    // Show the new tab, hide the rest, mark it active.
    activate(app, &label);

    let _ = app.emit(
        "tab:opened",
        serde_json::json!({ "label": label, "title": tab_title(&url) }),
    );
}

/// Show `label`'s webview at the content rect, hide every other tab, and
/// record it as active. Runs on the main thread.
fn activate(app: &AppHandle, label: &str) {
    let window = match app.get_window("main") {
        Some(w) => w,
        None => return,
    };
    let (lw, lh) = match logical_inner(&window) {
        Some(v) => v,
        None => return,
    };
    let content_h = (lh - TITLE_BAR_HEIGHT).max(0.0);
    let order = { app.state::<Mutex<TabState>>().lock().unwrap().order.clone() };
    for l in &order {
        if let Some(wv) = app.get_webview(l) {
            if l == label {
                let _ = wv.set_position(LogicalPosition::new(0.0, TITLE_BAR_HEIGHT));
                let _ = wv.set_size(LogicalSize::new(lw, content_h));
                let _ = wv.show();
            } else {
                let _ = wv.hide();
            }
        }
    }
    app.state::<Mutex<TabState>>().lock().unwrap().active = label.to_string();
}

/// Close a tab: drop its webview, re-activate a neighbor if it was the
/// active tab, and tell the title bar. The Platform tab can't be closed.
/// Runs on the main thread.
fn close_tab_impl(app: &AppHandle, label: &str) {
    if label == PLATFORM_TAB {
        return;
    }
    let reactivate = {
        let state = app.state::<Mutex<TabState>>();
        let mut s = state.lock().unwrap();
        if let Some(idx) = s.order.iter().position(|l| l == label) {
            s.order.remove(idx);
        }
        if s.active == label {
            // Fall back to the last remaining tab (or Platform).
            Some(
                s.order
                    .last()
                    .cloned()
                    .unwrap_or_else(|| PLATFORM_TAB.to_string()),
            )
        } else {
            None
        }
    };
    if let Some(wv) = app.get_webview(label) {
        let _ = wv.close();
    }
    if let Some(next) = &reactivate {
        activate(app, next);
    }
    let active_now = app.state::<Mutex<TabState>>().lock().unwrap().active.clone();
    let _ = app.emit(
        "tab:closed",
        serde_json::json!({ "label": label, "active": active_now }),
    );
}

/// Human label for a tab from its URL's last path segment.
fn tab_title(url: &Url) -> String {
    let seg = url
        .path()
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("Page");
    match seg {
        "pos" => "POS".to_string(),
        "messaging" => "Messaging".to_string(),
        "marketing" => "Marketing".to_string(),
        "waitlist" => "Waitlist".to_string(),
        "schedule" => "Schedule".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Page".to_string(),
            }
        }
    }
}

/// Title bar + active tab follow the window size.
fn relayout(app: &AppHandle) {
    let window = match app.get_window("main") {
        Some(w) => w,
        None => return,
    };
    let (lw, lh) = match logical_inner(&window) {
        Some(v) => v,
        None => return,
    };
    let content_h = (lh - TITLE_BAR_HEIGHT).max(0.0);
    if let Some(title_bar) = app.get_webview("title_bar") {
        let _ = title_bar.set_size(LogicalSize::new(lw, TITLE_BAR_HEIGHT));
    }
    let active = app.state::<Mutex<TabState>>().lock().unwrap().active.clone();
    if let Some(content) = app.get_webview(&active) {
        let _ = content.set_position(LogicalPosition::new(0.0, TITLE_BAR_HEIGHT));
        let _ = content.set_size(LogicalSize::new(lw, content_h));
    }
}

/// Window inner size in logical pixels.
fn logical_inner(window: &tauri::Window) -> Option<(f64, f64)> {
    let size: PhysicalSize<u32> = window.inner_size().ok()?;
    let scale = window.scale_factor().ok()?;
    Some(((size.width as f64) / scale, (size.height as f64) / scale))
}

// ─── IPC commands ───────────────────────────────────────────────────
// Invoked from the title bar via window.__TAURI__.core.invoke(...).

#[tauri::command]
fn switch_tab(app: AppHandle, label: String) {
    let _ = app.clone().run_on_main_thread(move || activate(&app, &label));
}

#[tauri::command]
fn close_tab(app: AppHandle, label: String) {
    let _ = app
        .clone()
        .run_on_main_thread(move || close_tab_impl(&app, &label));
}

#[tauri::command]
fn window_minimize(window: tauri::Window) -> tauri::Result<()> {
    window.minimize()
}

#[tauri::command]
fn window_toggle_maximize(window: tauri::Window) -> tauri::Result<()> {
    if window.is_maximized()? {
        window.unmaximize()
    } else {
        window.maximize()
    }
}

#[tauri::command]
fn window_close(window: tauri::Window) -> tauri::Result<()> {
    window.close()
}

#[tauri::command]
fn window_is_maximized(window: tauri::Window) -> tauri::Result<bool> {
    window.is_maximized()
}
