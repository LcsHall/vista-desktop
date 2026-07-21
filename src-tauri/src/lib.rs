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

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

// ─── Vista Voice (softphone) support ────────────────────────────────
//
// The Phone tab hosts platform.vistainterface.com/manage/<id>/voice, which
// embeds the Amazon Connect CCP (WebRTC). Three shell-side jobs:
//
//   1. Call-active plumbing: the page calls
//      window.__VISTA_SET_CALL_ACTIVE(active) on Streams contact events.
//      Same sentinel-navigation trick as the close bridge (the nav is
//      cancelled in on_navigation, so the page never actually leaves) —
//      deliberately NOT remote IPC: granting invoke to a remote origin
//      would expose every command; the sentinel can only flip a boolean.
//   2. Close guards: while a call is active, closing the window or the
//      voice tab would hang up on a client with no rejoin (Connect has no
//      mid-call reconnect). Both are intercepted; the title bar shows a
//      confirm strip ("Keep call" / "Close anyway").
//   3. Hidden-tab audio: WebView2 may throttle/occlude a hidden webview,
//      which could stall call audio. Voice tabs are therefore parked
//      OFF-SCREEN when inactive (still visible to the compositor → never
//      throttled) instead of hidden like ordinary tabs.
const CALL_SENTINEL: &str = "https://vista-desktop.invalid/__call_active__/";
// Fired by the voice page the moment the CCP agent session is live. The shell
// uses it to close the managed Connect sign-in window (see LOGIN_WINDOW):
// "agent ready" is the one authoritative signal that login truly finished —
// more reliable than watching the login window's own redirects.
const AGENT_READY_SENTINEL: &str = "https://vista-desktop.invalid/__agent_ready__";
// NOTE: these signals use window.open(), NOT location.href. A top-level
// navigation (even one on_navigation cancels) fires beforeunload first,
// and the CCP iframe registers a leave-warning handler — so every call
// start/end popped a native "Leave site?" dialog. window.open() routes
// through on_new_window instead (decide_new_window swallows sentinels)
// and never triggers unload machinery.
const VOICE_BRIDGE_SCRIPT: &str = r#"(function () {
  window.__VISTA_SET_CALL_ACTIVE = function (active) {
    window.open('https://vista-desktop.invalid/__call_active__/' + (active ? '1' : '0'));
  };
  window.__VISTA_AGENT_READY = function () {
    window.open('https://vista-desktop.invalid/__agent_ready__');
  };
})();"#;

// Label of the managed Connect sign-in window (Tauri-owned so the shell can
// close it deterministically — the engine-native popup WebView2 spawns for
// window.open could linger as a fully-functional duplicate CCP).
const LOGIN_WINDOW: &str = "connect-login";

// Whether a call is live right now (armed by the voice page via the call
// sentinel; cleared on end/destroy or when the voice tab is force-closed).
static CALL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True when a tab URL is the softphone page (/manage/<companyId>/voice).
fn is_voice_url(url: &Url) -> bool {
    url.path().rsplit('/').find(|s| !s.is_empty()) == Some("voice")
}

/// Hosts whose popups must stay inside WebView2 (shared cookie jar) so the
/// Connect login session lands where the CCP iframe can read it. Proven in
/// the ccp-spike: routing these to the system browser strands the login.
fn is_auth_popup_host(host: &str) -> bool {
    host.ends_with(".my.connect.aws")
        || host.ends_with(".awsapps.com")
        || host.ends_with(".aws.amazon.com")
        || host.ends_with(".amazonaws.com")
        || host.ends_with(".amazoncognito.com")
        || host == "signin.aws.amazon.com"
}

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
    /// Labels of tabs hosting the softphone page — these get the
    /// off-screen park (not hide) and the call-active close guard.
    voice: Vec<String>,
    /// Labels of Phone-settings tabs (deduped like voice tabs — re-opening
    /// focuses the existing one instead of stacking duplicates).
    settings: Vec<String>,
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
            let nav_plat_app = app.handle().clone();
            let content = WebviewBuilder::new(PLATFORM_TAB, WebviewUrl::External(prod_url))
                // Voice bridge included for uniformity: if the Platform tab
                // itself ever lands on the softphone page, the call-active
                // contract still works (the sidebar normally target=_blanks
                // it into its own tab).
                .initialization_script(format!(
                    "{}{}",
                    desktop_marker_script(),
                    VOICE_BRIDGE_SCRIPT
                ))
                .on_new_window(move |url, _features| decide_new_window(&nw_app, url))
                .on_navigation(move |nav_url| {
                    if let Some(flag) = nav_url.as_str().strip_prefix(CALL_SENTINEL) {
                        CALL_ACTIVE.store(flag.starts_with('1'), Ordering::SeqCst);
                        return false; // cancel the sentinel nav; page stays put
                    }
                    if nav_url.as_str().starts_with(AGENT_READY_SENTINEL) {
                        close_login_window(&nav_plat_app);
                        return false;
                    }
                    true
                });
            window.add_child(
                content,
                LogicalPosition::new(0.0, TITLE_BAR_HEIGHT),
                LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT - TITLE_BAR_HEIGHT),
            )?;

            // Mic auto-grant for the softphone (WebView2-level; no-op until
            // a trusted page actually asks for the microphone).
            #[cfg(windows)]
            attach_mic_handler(app.handle(), PLATFORM_TAB);

            // Seed tab state with the Platform tab active.
            app.manage(Mutex::new(TabState {
                order: vec![PLATFORM_TAB.to_string()],
                active: PLATFORM_TAB.to_string(),
                voice: Vec::new(),
                settings: Vec::new(),
            }));

            // Keep the title bar + the active tab sized to the window; block
            // a window close while a call is live (closing hangs up with no
            // rejoin — the title bar shows Keep call / Close anyway instead).
            let resize_app = app.handle().clone();
            window.on_window_event(move |event| {
                match event {
                    WindowEvent::Resized(_) => relayout(&resize_app),
                    WindowEvent::CloseRequested { api, .. } => {
                        if CALL_ACTIVE.load(Ordering::SeqCst) {
                            api.prevent_close();
                            let _ = resize_app.emit(
                                "call:close-blocked",
                                serde_json::json!({ "kind": "window" }),
                            );
                        }
                    }
                    _ => {}
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
            window_force_close,
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
/// goes: AWS/Connect auth popups open in a MANAGED Tauri window (same
/// WebView2 profile → same cookie jar the CCP session needs, but a window
/// the shell can deterministically close — the engine-native popup could
/// linger after login as a fully-functional duplicate CCP); an internal
/// /manage/* page on the Platform host opens as a new tab; everything
/// else opens in the system browser.
fn decide_new_window(app: &AppHandle, url: Url) -> NewWindowResponse<tauri::Wry> {
    // Voice-bridge sentinels arrive as window.open() (see VOICE_BRIDGE_SCRIPT
    // beforeunload note) — consume them here, open nothing.
    if let Some(flag) = url.as_str().strip_prefix(CALL_SENTINEL) {
        CALL_ACTIVE.store(flag.starts_with('1'), Ordering::SeqCst);
        return NewWindowResponse::Deny;
    }
    if url.as_str().starts_with(AGENT_READY_SENTINEL) {
        let app = app.clone();
        let _ = app
            .clone()
            .run_on_main_thread(move || close_login_window(&app));
        return NewWindowResponse::Deny;
    }
    if is_auth_popup_host(url.host_str().unwrap_or("")) {
        let app = app.clone();
        let _ = app
            .clone()
            .run_on_main_thread(move || open_login_window(&app, url));
        return NewWindowResponse::Deny;
    }
    route_new_window(app, url);
    NewWindowResponse::Deny
}

/// Open (or focus) the managed Connect sign-in window. Closed by the
/// agent-ready sentinel once the softphone session is live, so it can only
/// ever exist while a sign-in is genuinely in progress.
fn open_login_window(app: &AppHandle, url: Url) {
    if let Some(existing) = app.get_webview_window(LOGIN_WINDOW) {
        let _ = existing.set_focus();
        return;
    }
    let result = tauri::WebviewWindowBuilder::new(app, LOGIN_WINDOW, WebviewUrl::External(url))
        .title("Sign in — Vista Phone")
        .inner_size(480.0, 700.0)
        .center()
        .build();
    if let Err(e) = result {
        eprintln!("[vista-desktop] failed to open login window: {e}");
    }
}

/// Close the managed sign-in window (agent session is live → login done).
fn close_login_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LOGIN_WINDOW) {
        let _ = w.close();
    }
}

/// Open an internal /manage/* page as a tab, anything else in the system
/// browser. Window/webview mutations are deferred to the main thread to
/// avoid reentrancy while the webview engine is mid new-window-request.
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

    // Dedupe the singleton tabs: re-opening Phone or Phone settings (sidebar
    // re-click, click-to-call fallback) FOCUSES the existing tab instead of
    // stacking a duplicate — two live CCPs would be two competing softphones.
    let is_settings_url =
        url.path().rsplit('/').find(|s| !s.is_empty()) == Some("voice-settings");
    if is_voice_url(&url) || is_settings_url {
        let candidates = {
            let s = app.state::<Mutex<TabState>>();
            let s = s.lock().unwrap();
            if is_settings_url { s.settings.clone() } else { s.voice.clone() }
        };
        for l in candidates {
            if app.get_webview(&l).is_some() {
                activate(app, &l);
                let _ = app.emit("tab:focused", serde_json::json!({ "label": l }));
                return;
            }
        }
    }

    let label = format!("tab-{}", TAB_COUNTER.fetch_add(1, Ordering::Relaxed));
    let (lw, lh) = logical_inner(&window).unwrap_or((INITIAL_WIDTH, INITIAL_HEIGHT));
    let content_h = (lh - TITLE_BAR_HEIGHT).max(0.0);

    let nw_app = app.clone();
    let nav_app = app.clone();
    let nav_label = label.clone();
    let builder = WebviewBuilder::new(label.as_str(), WebviewUrl::External(url.clone()))
        // Tabs can themselves open further tabs / auth popups / external links.
        .on_new_window(move |u, _features| decide_new_window(&nw_app, u))
        // Mark the desktop app, bridge window.close() → sentinel nav, and
        // give the softphone page its call-active hook.
        .initialization_script(format!(
            "{}{}{}",
            desktop_marker_script(),
            CLOSE_BRIDGE_SCRIPT,
            VOICE_BRIDGE_SCRIPT
        ))
        .on_navigation(move |nav_url| {
            if nav_url.as_str().starts_with(CLOSE_SENTINEL) {
                let a = nav_app.clone();
                let l = nav_label.clone();
                let _ = nav_app.run_on_main_thread(move || close_tab_impl(&a, &l, false));
                return false; // cancel the sentinel nav; the tab is closing
            }
            if let Some(flag) = nav_url.as_str().strip_prefix(CALL_SENTINEL) {
                CALL_ACTIVE.store(flag.starts_with('1'), Ordering::SeqCst);
                return false; // cancel the sentinel nav; the page stays put
            }
            if nav_url.as_str().starts_with(AGENT_READY_SENTINEL) {
                close_login_window(&nav_app);
                return false;
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

    // Softphone mic auto-grant (no-op unless a trusted page asks).
    #[cfg(windows)]
    attach_mic_handler(app, &label);

    {
        let state = app.state::<Mutex<TabState>>();
        let mut s = state.lock().unwrap();
        s.order.push(label.clone());
        if is_voice_url(&url) {
            s.voice.push(label.clone());
        } else if is_settings_url {
            s.settings.push(label.clone());
        }
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
    let (order, voice) = {
        let s = app.state::<Mutex<TabState>>();
        let s = s.lock().unwrap();
        (s.order.clone(), s.voice.clone())
    };
    for l in &order {
        if let Some(wv) = app.get_webview(l) {
            if l == label {
                let _ = wv.set_position(LogicalPosition::new(0.0, TITLE_BAR_HEIGHT));
                let _ = wv.set_size(LogicalSize::new(lw, content_h));
                let _ = wv.show();
            } else if voice.iter().any(|v| v == l) {
                // Voice tabs are parked OFF-SCREEN instead of hidden: a
                // hidden (IsVisible=false) WebView2 can be throttled or
                // occluded, which risks stalling live call audio. Parked
                // far left it keeps compositing normally, receives no
                // input (zero overlap with the window), and the call
                // keeps flowing while the user works in other tabs.
                let _ = wv.show();
                let _ = wv.set_position(LogicalPosition::new(-(lw + 4000.0), TITLE_BAR_HEIGHT));
            } else {
                let _ = wv.hide();
            }
        }
    }
    app.state::<Mutex<TabState>>().lock().unwrap().active = label.to_string();
}

/// Close a tab: drop its webview, re-activate a neighbor if it was the
/// active tab, and tell the title bar. The Platform tab can't be closed.
/// A voice tab with a live call refuses to close unless `force` (the
/// title-bar confirm strip's "Close anyway") — closing it hangs up on the
/// client with no rejoin. Runs on the main thread.
fn close_tab_impl(app: &AppHandle, label: &str, force: bool) {
    if label == PLATFORM_TAB {
        return;
    }
    let is_voice = {
        let state = app.state::<Mutex<TabState>>();
        let s = state.lock().unwrap();
        s.voice.iter().any(|v| v == label)
    };
    if is_voice && !force && CALL_ACTIVE.load(Ordering::SeqCst) {
        let _ = app.emit(
            "call:close-blocked",
            serde_json::json!({ "kind": "tab", "label": label }),
        );
        return;
    }
    let reactivate = {
        let state = app.state::<Mutex<TabState>>();
        let mut s = state.lock().unwrap();
        if let Some(idx) = s.order.iter().position(|l| l == label) {
            s.order.remove(idx);
        }
        s.voice.retain(|v| v != label);
        s.settings.retain(|v| v != label);
        // The page that armed the call flag is going away — clear it so a
        // stale flag can't block closes forever.
        if is_voice {
            CALL_ACTIVE.store(false, Ordering::SeqCst);
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
        // The softphone page — labeled how the front desk talks about it.
        "voice" => "Phone".to_string(),
        "voice-settings" => "Phone settings".to_string(),
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
fn close_tab(app: AppHandle, label: String, force: Option<bool>) {
    let force = force.unwrap_or(false);
    let _ = app
        .clone()
        .run_on_main_thread(move || close_tab_impl(&app, &label, force));
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

/// Bypass the call-active close guard once the user has confirmed
/// "Close anyway" in the title bar's confirm strip. destroy() skips
/// CloseRequested entirely, so the guard can't re-block it.
#[tauri::command]
fn window_force_close(window: tauri::Window) -> tauri::Result<()> {
    window.destroy()
}

#[tauri::command]
fn window_is_maximized(window: tauri::Window) -> tauri::Result<bool> {
    window.is_maximized()
}

// ─── Softphone mic auto-grant (WebView2) ────────────────────────────

/// Auto-grant Microphone at the WebView2 level for trusted origins.
///
/// WebView2 shows its own mic prompt and a user's "Block" is sticky with
/// no re-prompt path — unacceptable for a front-desk softphone. wry only
/// registers a PermissionRequested handler for clipboard-read, so this
/// handler is the sole decider for Microphone. Trusted origins: the
/// Platform app itself (with allowFramedSoftphone, Chromium permission
/// delegation reports the TOP-LEVEL origin) plus the Connect frame hosts
/// as a fallback. Everything else keeps the default prompt.
///
/// Known caveat (WebView2Feedback#4740, observed in the 2026-07-19
/// ccp-spike): SetHandled(true) doesn't suppress the default dialog on
/// every runtime version — "granted silently OR a single prompt" is the
/// accepted outcome.
#[cfg(windows)]
fn attach_mic_handler(app: &AppHandle, label: &str) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2PermissionRequestedEventArgs2, COREWEBVIEW2_PERMISSION_KIND,
        COREWEBVIEW2_PERMISSION_KIND_MICROPHONE, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
    };
    use webview2_com::{take_pwstr, PermissionRequestedEventHandler};
    use windows::core::{Interface, PWSTR};

    let Some(webview) = app.get_webview(label) else {
        return;
    };
    let result = webview.with_webview(move |platform_webview| {
        // SAFETY: all WebView2 COM calls happen on the UI thread that
        // with_webview schedules onto; the handler fires on that thread too.
        unsafe {
            let controller = platform_webview.controller();
            let core = match controller.CoreWebView2() {
                Ok(core) => core,
                Err(e) => {
                    eprintln!("[vista-desktop:mic] CoreWebView2() failed: {e}");
                    return;
                }
            };
            let handler =
                PermissionRequestedEventHandler::create(Box::new(move |_sender, args| {
                    let Some(args) = args else { return Ok(()) };
                    let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
                    args.PermissionKind(&mut kind)?;
                    if kind != COREWEBVIEW2_PERMISSION_KIND_MICROPHONE {
                        return Ok(());
                    }
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    let uri = take_pwstr(uri);
                    let trusted = uri.starts_with("https://platform.vistainterface.com")
                        || uri.contains(".my.connect.aws")
                        || uri.contains(".awsapps.com");
                    if trusted {
                        args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                        // Best effort — see the #4740 caveat above.
                        let _ = args
                            .cast::<ICoreWebView2PermissionRequestedEventArgs2>()
                            .and_then(|a2| a2.SetHandled(true));
                    }
                    Ok(())
                }));
            let mut token: i64 = 0;
            if let Err(e) = core.add_PermissionRequested(&handler, &mut token) {
                eprintln!("[vista-desktop:mic] add_PermissionRequested failed: {e}");
            }
        }
    });
    if let Err(e) = result {
        eprintln!("[vista-desktop:mic] with_webview failed for {label}: {e}");
    }
}
