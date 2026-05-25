// Vista Platform — Tauri shell.
//
// Architecture: one borderless window with TWO child webviews stacked
// vertically:
//   ┌────────────────────────────────┐
//   │ title bar (local index.html)   │  40 px tall, draggable
//   ├────────────────────────────────┤
//   │ content (app.vistainterface)   │  fills the rest
//   │                                │
//   └────────────────────────────────┘
//
// The title bar lives in this repo (HTML/CSS in /src) so the desktop
// shell stays self-contained. The content webview points at the live
// production app, so any web change ships instantly via Vercel — no
// EAS-style rebuild for content tweaks. The shell itself only needs
// updates when we change native features (tray, menus, icons,
// auto-updater plumbing).
//
// Why multi-webview instead of iframe? An iframe would be subject to
// X-Frame-Options / CSP frame-ancestors restrictions, and Supabase
// auth popups (OAuth providers, etc.) would be awkward. Multi-webview
// gives the content a top-level browsing context — same as a real
// browser window — so auth flows + cookies + websockets all work
// exactly as they do at https://app.vistainterface.com in Chrome.

// `Manager` brings the `get_webview` / webview-lookup methods into scope
// (trait methods, unstable-gated alongside the multi-webview API).
use tauri::webview::NewWindowResponse;
use tauri::{
    LogicalPosition, LogicalSize, Manager, PhysicalSize, WebviewBuilder, WebviewUrl, WindowBuilder,
    WindowEvent,
};

// Production URL the content webview loads. MUST be the Platform host,
// not app.vistainterface.com.
//
// Login lives on Interface (app.vistainterface.com/login). When an
// unauthenticated user hits platform.vistainterface.com, Platform's
// proxy sets a `vista_postlogin_target` cookie on `.vistainterface.com`
// and *then* redirects to the Interface login. The Interface login
// reads that cookie after a successful sign-in and hands the session
// back to Platform (cross-subdomain handoff). Loading
// app.vistainterface.com directly skips the cookie-setting redirect, so
// login has no post-login target and dumps the user on Interface
// instead of Platform. The extra redirect hop is load-bearing — do not
// "optimize" it away. (Cookies on `.vistainterface.com` are shared
// across both subdomains within the WebView2 profile, so the handoff
// works exactly as it does in a desktop browser.)
const PRODUCTION_URL: &str = "https://platform.vistainterface.com";

// Height of the custom title bar in logical pixels. Matches modern
// Windows app conventions (Edge, Teams, Notion all sit around 32-40).
const TITLE_BAR_HEIGHT: f64 = 40.0;

// Initial window size. Picked to comfortably fit the vista-platform
// layout's max-width content areas + sidebar. User can resize freely.
const INITIAL_WIDTH: f64 = 1280.0;
const INITIAL_HEIGHT: f64 = 800.0;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Updater plugin: configured in tauri.conf.json. Polls the
        // GitHub Releases manifest on launch + offers any newer
        // signed build via a confirmation dialog.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let window = WindowBuilder::new(app, "main")
                .title("Vista Platform")
                .inner_size(INITIAL_WIDTH, INITIAL_HEIGHT)
                .min_inner_size(900.0, 600.0)
                // decorations(false) = no native title bar / no
                // Windows chrome. We draw our own (see /src/index.html).
                .decorations(false)
                // Center on first launch. After that, Tauri restores
                // the last position via the optional window-state
                // plugin if we add one later.
                .center()
                .build()?;

            // Title bar webview — loads /src/index.html. WebviewUrl::App
            // resolves to the frontendDist directory configured in
            // tauri.conf.json.
            window.add_child(
                WebviewBuilder::new("title_bar", WebviewUrl::App("index.html".into())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(INITIAL_WIDTH, TITLE_BAR_HEIGHT),
            )?;

            // Content webview — loads the live production app.
            // Positioned just below the title bar; resize handler
            // below keeps it sized to the window.
            let prod_url = PRODUCTION_URL
                .parse()
                .expect("PRODUCTION_URL must be a valid URL");
            let content = WebviewBuilder::new("content", WebviewUrl::External(prod_url))
                // A desktop window has no browser tabs, so any
                // target="_blank" / window.open request (the sidebar's
                // "View Booking Site" + "VISTA Consulting" links, plus
                // any other external link in the web app) would otherwise
                // try to spawn a chromeless child window. Instead, hand
                // the URL to the OS default browser and deny the in-app
                // window. Note: this also intercepts any window.open the
                // app uses for OAuth popups — fine today since sign-in is
                // email/password; revisit if a provider popup is added.
                .on_new_window(|url, _features| {
                    if let Err(e) = open::that_detached(url.as_str()) {
                        eprintln!("[vista-desktop] failed to open {url} in browser: {e}");
                    }
                    NewWindowResponse::Deny
                });
            window.add_child(
                content,
                LogicalPosition::new(0.0, TITLE_BAR_HEIGHT),
                LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT - TITLE_BAR_HEIGHT),
            )?;

            // Resize handler: when the window changes size, resize
            // the content webview so it fills the area below the
            // title bar. Title bar stays at its 40px height + full
            // width.
            let window_handle = window.clone();
            window.on_window_event(move |event| {
                if let WindowEvent::Resized(_) = event {
                    if let Err(e) = resize_webviews(&window_handle) {
                        eprintln!("[vista-desktop] resize failed: {e}");
                    }
                }
            });

            // First resize after the window is fully on-screen — the
            // initial size set in WindowBuilder is sometimes adjusted
            // by the OS before the webviews attach, leaving the
            // content area slightly off. Force one resize now to
            // line everything up.
            resize_webviews(&window)?;

            Ok(())
        })
        // IPC commands invoked from the title bar HTML to control the
        // window. See /src/main.ts for the JS side.
        .invoke_handler(tauri::generate_handler![
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_is_maximized,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Resize the two child webviews to match the current window size.
/// Title bar stays its fixed height; content fills the rest.
fn resize_webviews(window: &tauri::Window) -> tauri::Result<()> {
    let size: PhysicalSize<u32> = window.inner_size()?;
    let scale = window.scale_factor()?;
    let logical_width = (size.width as f64) / scale;
    let logical_height = (size.height as f64) / scale;
    let content_height = (logical_height - TITLE_BAR_HEIGHT).max(0.0);

    if let Some(title_bar) = window.get_webview("title_bar") {
        title_bar.set_size(LogicalSize::new(logical_width, TITLE_BAR_HEIGHT))?;
    }
    if let Some(content) = window.get_webview("content") {
        content.set_position(LogicalPosition::new(0.0, TITLE_BAR_HEIGHT))?;
        content.set_size(LogicalSize::new(logical_width, content_height))?;
    }
    Ok(())
}

// ─── Window control IPC commands ────────────────────────────────────
// These are invoked by the title bar HTML's min/max/close buttons via
// the Tauri JS API: `window.__TAURI__.core.invoke('window_minimize')`.

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
