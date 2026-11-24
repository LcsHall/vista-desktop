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
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;
// `Manager` brings get_webview / get_window / state / manage into scope;
// `Emitter` brings emit (title-bar notifications). Both, plus the
// multi-webview API (WindowBuilder / WebviewBuilder / Window::add_child /
// get_webview), are unstable-gated — keep `features=["unstable"]`.
use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, PhysicalSize, RunEvent, Url,
    WebviewBuilder, WebviewUrl, WindowBuilder, WindowEvent,
};

// Phone-tab self-recovery (v0.3.3): the shell's safety net for a Phone tab
// whose page never arrived or died (see phone_recovery.rs).
mod phone_recovery;

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
// a new tab) apart from "external link" (open in the system browser). One
// definition, shared with the Phone-tab recovery (v0.3.3), so the address it
// rebuilds and the pages it recognises can never disagree with the shell.
const PLATFORM_HOST: &str = phone_recovery::PLATFORM_HOST;

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

// ─── Native dialog globals (confirm / alert / prompt) ───────────────
//
// COMPENSATES FOR: tauri-plugin-dialog 2.x (vendored + read here at 2.7.1).
//
// That plugin's `init()` unconditionally injects `src/init-iife.js` on every
// non-Android target — there is no opt-out — and that script does:
//
//     window.alert   = function (m) { invoke('plugin:dialog|message', ...) }
//     window.confirm = async function (m) { return await invoke('plugin:dialog|confirm', ...) }
//
// Two independent defects follow, and the confirm one is a DATA-LOSS bug:
//
//  1. `confirm` is now an ASYNC function, so it returns a Promise — and a
//     Promise is ALWAYS truthy. Every guard in the web apps inverts:
//         if (!confirm('Are you sure?')) return;   // never returns
//         if (confirm('Delete?')) doDelete();      // always deletes
//     ~167 confirm-gated destructive actions reachable from this shell
//     therefore execute IMMEDIATELY, with no prompt shown. Worse, the
//     plugin registers NO `confirm` command at all — its generate_handler!
//     lists only open/save/message — so the invoke can never succeed.
//  2. `alert` maps to `plugin:dialog|message`, which IS a real command, but
//     our capability grants no `remote` origin access and every content
//     webview loads a remote https:// origin, so the invoke is denied by the
//     ACL. ~206 alert() calls display NOTHING and swallow a rejected promise.
//
// `prompt` is NOT touched by 2.7.1; it is covered here purely so a future
// plugin version can't start overriding it without us noticing.
//
// ─── WHAT PRODUCTION ACTUALLY TAUGHT US ─────────────────────────────
//
// OBSERVED, v0.1.4 (2026-07-17) through v0.2.5: confirm fails OPEN. Reported
// repeatedly; matches defect 1 above.
//
// OBSERVED, v0.2.6 on the owner's machine (2026-08-10), verbatim: "nothing
// happens when I click the x. there is no pop up, the note stays." The test
// was `if (!confirm('Delete this note? This cannot be undone.')) return`, so
// confirm returned FALSY with no dialog — the fail-closed branch. That means
// v0.2.6's `native('confirm')` resolved to null: BOTH of its sources failed
// inside the real WebView2.
//
// ROOT CAUSE, established in the vendored sources (not theorised):
//   * wry 0.55.1 `src/lib.rs:2495` documents, on `InitializationScript`:
//       "**Windows**: scripts are always injected into subframes regardless
//        of this option. This will be the case until Webview2 implements a
//        proper API to inject a script only on the main frame."
//     and `src/webview2/mod.rs:492` is literally commented "Initialize main
//     and subframe scripts" — it loops EVERY init script through
//     `ICoreWebView2::AddScriptToExecuteOnDocumentCreated`, which WebView2
//     applies to every frame. `for_main_frame_only: true` is INERT on Windows.
//   * Therefore the dialog plugin's override ALSO runs inside the
//     same-origin about:blank child frame v0.2.6 created. isNative() correctly
//     rejected it, native() returned null, confirm fell closed. Hypothesis (a).
//   * The `Window.prototype` source was already known dead (WebIDL [Global]
//     installs alert/confirm/prompt as OWN properties of the global; measured:
//     `typeof Window.prototype.confirm === 'undefined'` in Chromium). So both
//     of v0.2.6's sources were gone and there was nothing left.
//   * WHY THE EDGE-151/CDP MEASUREMENT PASSED AND THE SHIP FAILED: the CDP
//     harness injected the scripts into the MAIN FRAME ONLY (that is all
//     Page.addScriptToEvaluateOnNewDocument did there without
//     `includeCommandLineAPI`/frame fan-out). It never exercised wry's
//     inject-everywhere behaviour, so it could not reproduce the pollution.
//     A green measurement on a different injector is not a test of this one.
//
// ─── THE MECHANISM NOW ──────────────────────────────────────────────
//
// Stop hunting for an unpolluted realm; there isn't one. Capture the natives
// BEFORE the plugin can overwrite them, in this realm, and hand them forward.
//
//   * `DIALOG_CAPTURE_SCRIPT` ships as a tiny local Tauri plugin registered
//     BEFORE `tauri_plugin_dialog::init()` (see run()). Verified in the
//     vendored tauri 2.11.5 source that this ordering is real:
//       - `Builder::plugin_boxed` (app.rs:1859) -> `PluginStore::register`
//         (plugin.rs:879) does `self.store.push(plugin)` — a Vec, so
//         REGISTRATION ORDER is preserved (it only `retain`s away a plugin of
//         the SAME name; ours is unique).
//       - `PluginStore::initialization_script` (plugin.rs:918) iterates
//         `self.store.iter()` in that order and wraps each in
//         `(function () { ... })();`.
//       - `manager/webview.rs:202` extends the script list with those plugin
//         scripts, then :223 appends `webview_attributes.initialization_scripts`
//         — so builder scripts (DIALOG_GLOBALS_SCRIPT) still run LAST.
//     Net order per document: [capture] ... [dialog plugin] ... [restore].
//   * The capture script stashes the pristine, `window`-bound functions on a
//     NON-CONFIGURABLE, NON-WRITABLE, non-enumerable own property of the
//     global (`__VISTA_NATIVE_DIALOGS__`). A page cannot reassign or delete
//     it, and the dialog plugin never touches it. It also immediately installs
//     a minimal accessor over confirm/alert/prompt so the fix holds even if
//     the restore script never runs.
//   * `DIALOG_GLOBALS_SCRIPT` then resolves each global through LAYERED
//     sources, best first, every one gated by isNative():
//         stash -> own property -> Window.prototype -> about:blank frame -> null
//     `own` catches the case where nothing overrode it at all (2.7.1 leaves
//     `prompt` alone). `proto` and `frame` are v0.2.6's sources, kept: they
//     cost nothing, they may hold on another engine, and the frame source now
//     also reads the CHILD realm's own stash (which exists precisely because
//     Windows injects into subframes).
//   * FAIL CLOSED remains the terminal state: no native => confirm returns
//     false, prompt returns null, alert shows nothing. The owner's report
//     proves that branch works and is safe — a note was not deleted.
//
// ORDER-INDEPENDENCE (both directions are safe, deliberately):
//   * If the capture somehow runs AFTER the plugin, it sees a non-native
//     `confirm`, stashes nothing, and RECORDS `pre.confirm = 'overridden'` —
//     the diagnostic then names that as the failure, instead of us guessing.
//   * If a future tauri runs builder scripts BEFORE plugin scripts, the
//     accessor's setter silently swallows the plugin's `window.confirm = ...`.
//     A frozen data property would also block it but would THROW inside the
//     plugin's strict-mode IIFE; a no-op setter does not.
//
// ─── SELF-DIAGNOSTIC (item 3 — no more blind shipping) ──────────────
//
// Every resolution is recorded on `window.__VISTA_DIALOG_DIAG__`: which source
// satisfied each global, why each earlier source was rejected, and the last 12
// dialog calls with their outcome. Two ways to read it WITHOUT devtools:
//   * Ctrl+Alt+Shift+D toggles an overlay in the top frame, with a Copy button.
//   * Any FAIL-CLOSED call auto-opens that same overlay, once per document —
//     i.e. the next time "nothing happens when I click the x", the app says
//     exactly which sources it tried and how each one failed.
// Invisible in normal use: when a native resolves, nothing ever renders.
// NO MESSAGE TEXT IS EVER RECORDED — only booleans/'string'/'shown' — so the
// overlay cannot leak PHI from a confirm prompt.
//
// ⚠️ RE-CHECK ON ANY tauri / wry / tauri-plugin-dialog BUMP. Note that
// `src-tauri/Cargo.lock` is GITIGNORED, so CI re-resolves `tauri = "2"` and
// `tauri-plugin-dialog = "2"` on every build and the shipped versions are NOT
// pinned by this repo. Verify:
//   1. Does the plugin still inject init-iife.js, and does it still override
//      confirm/alert (and now prompt)? If upstream fixed it, delete all this.
//   2. Does `generate_handler!` in the plugin's lib.rs register a real
//      `confirm` command now?
//   3. Is `PluginStore::register` still a Vec push (registration order) and
//      `initialization_script` still an in-order iter? If it ever becomes a
//      map/set, the capture-runs-first guarantee is gone — the diagnostic will
//      say `pre=overridden` and only the accessor-setter half still holds.
//   4. Is the plugin-scripts-then-builder-scripts order in
//      manager/webview.rs unchanged? (If it flips, the accessor covers us.)
//   5. Does wry still inject into subframes on Windows (lib.rs:2495)? If
//      WebView2 ever grows real main-frame-only injection, the `frame` source
//      becomes load-bearing again and the stash becomes the redundant one.
// NOTE: adding `dialog:allow-confirm`/`dialog:allow-ask` to the capability is
// NOT a fix and was already tried — in 2.7.1 both are DEPRECATED ALIASES for
// the `message` command that `dialog:default` already grants. No ACL entry can
// authorise a command the plugin never registers.

// Name of the local capture plugin. MUST be registered before
// tauri_plugin_dialog::init(); MUST stay unique (PluginStore::register drops
// any earlier plugin with the same name).
const DIALOG_CAPTURE_PLUGIN: &str = "vista-dialog-capture";

// Runs FIRST in every document of every webview (including title_bar and the
// Connect sign-in window, which get no builder script of their own — so they
// are covered by this alone). Grabs the pristine dialog natives before the
// dialog plugin's init-iife.js can overwrite them.
const DIALOG_CAPTURE_SCRIPT: &str = r#"(function () {
  'use strict';

  var STASH = '__VISTA_NATIVE_DIALOGS__';
  var NAMES = ['confirm', 'alert', 'prompt'];

  function isNative(fn) {
    try {
      return typeof fn === 'function' &&
        Function.prototype.toString.call(fn).indexOf('[native code]') !== -1;
    } catch (e) { return false; }
  }

  // Idempotent. A document-start script can be replayed on the same document;
  // an earlier stash is by definition the more pristine one, so never redo it.
  try { if (window[STASH]) return; } catch (e) { return; }

  var fns = {};
  var pre = {};
  for (var i = 0; i < NAMES.length; i++) {
    var n = NAMES[i];
    var raw = null;
    try { raw = window[n]; } catch (e) { raw = null; }
    if (isNative(raw)) {
      pre[n] = 'native';
      try { fns[n] = raw.bind(window); } catch (e) { pre[n] = 'bindfail'; }
    } else if (typeof raw === 'function') {
      // Someone already replaced it => we did NOT run first. Recorded so the
      // diagnostic can say so out loud instead of us guessing again.
      pre[n] = 'overridden';
    } else {
      pre[n] = 'missing';
    }
  }

  var stash = { fns: fns, pre: pre };
  try { Object.freeze(fns); Object.freeze(pre); Object.freeze(stash); } catch (e) {}

  // Non-configurable + non-writable: the page cannot reassign or delete it.
  var ok = false;
  try {
    Object.defineProperty(window, STASH,
      { value: stash, writable: false, enumerable: false, configurable: false });
    ok = true;
  } catch (e) {}
  if (!ok) {
    try {
      Object.defineProperty(window, STASH,
        { value: stash, writable: false, enumerable: false, configurable: true });
      ok = true;
    } catch (e) {}
  }
  if (!ok) { try { window[STASH] = stash; } catch (e) {} }

  // Minimal early install, so the fix holds even if DIALOG_GLOBALS_SCRIPT
  // never runs (title bar / sign-in window) or throws. The accessor's setter
  // swallows the dialog plugin's `window.confirm = ...` a few scripts later.
  // enumerable:true matches the native WebIDL [Global] property it replaces.
  function text(v) { return v === undefined || v === null ? '' : String(v); }

  function pin(name, impl) {
    try {
      Object.defineProperty(window, name, {
        configurable: true,
        enumerable: true,
        get: function () { return impl; },
        set: function () { /* swallow the plugin's reassignment; must not throw */ }
      });
    } catch (e) {
      try { window[name] = impl; } catch (e2) {}
    }
  }

  if (fns.confirm) {
    pin('confirm', function (m) {
      try { return fns.confirm(text(m)) === true; } catch (e) { return false; }
    });
  }
  if (fns.alert) {
    pin('alert', function (m) {
      try { fns.alert(text(m)); } catch (e) {}
    });
  }
  if (fns.prompt) {
    pin('prompt', function (m, d) {
      try {
        return arguments.length > 1 ? fns.prompt(text(m), text(d)) : fns.prompt(text(m));
      } catch (e) { return null; }
    });
  }
})();"#;

// Runs LAST in content/tab webviews (WebviewBuilder::initialization_script).
// Resolves each dialog global through the layered sources and records what it
// found on window.__VISTA_DIALOG_DIAG__.
const DIALOG_GLOBALS_SCRIPT: &str = r#"(function () {
  'use strict';

  var STASH = '__VISTA_NATIVE_DIALOGS__';
  var DIAG = '__VISTA_DIALOG_DIAG__';
  var OVERLAY_ID = '__vista_dialog_diag_overlay__';
  var NAMES = ['confirm', 'alert', 'prompt'];

  function isNative(fn) {
    try {
      return typeof fn === 'function' &&
        Function.prototype.toString.call(fn).indexOf('[native code]') !== -1;
    } catch (e) { return false; }
  }

  function text(v) { return v === undefined || v === null ? '' : String(v); }

  var isTop = true;
  try { isTop = window.top === window; } catch (e) { isTop = false; }

  // ── Source 1: the pre-plugin stash (DIALOG_CAPTURE_SCRIPT) ────────
  function fromStash(name) {
    var s = null;
    try { s = window[STASH]; } catch (e) { return { fn: null, why: 'throw' }; }
    if (!s) return { fn: null, why: 'absent' };
    var fn = null;
    try { fn = s.fns ? s.fns[name] : null; } catch (e) {}
    if (isNative(fn)) return { fn: fn, why: 'ok' };
    var pre = '?';
    try { pre = (s.pre && s.pre[name]) || '?'; } catch (e) {}
    return { fn: null, why: 'pre=' + pre };
  }

  // ── Source 2: our own global, if nothing ever replaced it ─────────
  // Read via the descriptor so we never invoke an accessor we installed
  // ourselves (which would hand back a non-native wrapper).
  function fromOwn(name) {
    var d = null;
    try { d = Object.getOwnPropertyDescriptor(window, name); }
    catch (e) { return { fn: null, why: 'throw' }; }
    if (!d) return { fn: null, why: 'absent' };
    if (!('value' in d)) return { fn: null, why: 'accessor' };
    if (!isNative(d.value)) return { fn: null, why: 'replaced' };
    try { return { fn: d.value.bind(window), why: 'ok' }; }
    catch (e) { return { fn: null, why: 'bindfail' }; }
  }

  // ── Source 3: Window.prototype ────────────────────────────────────
  // Measured DEAD on Chromium (WebIDL [Global] => own properties, never the
  // prototype). One property read; kept for engines that differ.
  function fromProto(name) {
    var fn = null;
    try {
      var proto = Object.getPrototypeOf(window);
      fn = proto ? proto[name] : null;
    } catch (e) { return { fn: null, why: 'throw' }; }
    if (!isNative(fn)) return { fn: null, why: 'absent' };
    try { return { fn: fn.bind(window), why: 'ok' }; }
    catch (e) { return { fn: null, why: 'bindfail' }; }
  }

  // ── Source 4: a same-origin about:blank child realm ───────────────
  // v0.2.6's load-bearing path; now known to be POLLUTED on Windows, because
  // wry injects every init script into subframes there. Kept because it costs
  // nothing and may hold elsewhere — and because that same fan-out means the
  // child realm carries its own stash, which we prefer over its raw globals.
  var frameEl = null;
  function frameWin() {
    // MAIN FRAME ONLY — this guard is load-bearing, do not drop it.
    // On Windows wry fans EVERY init script into EVERY frame, so the
    // about:blank child created below runs this very script, and its
    // probe() would create a child of its own, and so on. Measured in
    // Chromium: uncapped, that chain hangs the renderer synchronously on
    // the first confirm(); depth-capped, it reached 7. The main frame is
    // also the only frame that needs this source — on Windows a subframe
    // already carries the capture stash, and on other platforms a subframe
    // never receives this script at all.
    if (!isTop) return null;
    try {
      if (frameEl && frameEl.isConnected && frameEl.contentWindow) return frameEl.contentWindow;
      var root = document.body || document.documentElement;
      if (!root) return null;
      frameEl = document.createElement('iframe');
      frameEl.setAttribute('aria-hidden', 'true');
      frameEl.setAttribute('tabindex', '-1');
      frameEl.style.cssText =
        'position:absolute;left:-9999px;top:0;width:1px;height:1px;border:0;';
      root.appendChild(frameEl);
      return frameEl.contentWindow;
    } catch (e) { return null; }
  }

  function fromFrame(name) {
    var w = frameWin();
    if (!w) return { fn: null, why: 'nowin' };
    try {
      var s = w[STASH];
      var sf = s && s.fns ? s.fns[name] : null;
      if (isNative(sf)) return { fn: sf, why: 'ok-stash' };
    } catch (e) {}
    var fn = null;
    try { fn = w[name]; } catch (e) { return { fn: null, why: 'throw' }; }
    if (!isNative(fn)) return { fn: null, why: 'polluted' };
    try { return { fn: fn.bind(w), why: 'ok' }; }
    catch (e) { return { fn: null, why: 'bindfail' }; }
  }

  var SOURCES = [
    ['stash', fromStash],
    ['own', fromOwn],
    ['proto', fromProto],
    ['frame', fromFrame]
  ];

  // ── Diagnostic state ──────────────────────────────────────────────
  var diag = {
    app: null,
    top: isTop,
    url: '',
    stash: 'absent',
    source: { confirm: null, alert: null, prompt: null },
    tried: { confirm: null, alert: null, prompt: null },
    calls: []
  };
  try { diag.app = window.__VISTA_DESKTOP__ || null; } catch (e) {}
  // Route SHAPE only — never the ids, never the query string. This overlay
  // exists to be screenshotted and sent to support, and a
  // /manage/<companyId>/clients/<clientId> path carries record identifiers
  // that must not leave the BAA boundary. The shell has no address bar, so
  // this overlay is the ONLY place a URL becomes visible and copyable.
  try {
    var path = String(location.pathname)
      .replace(/[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/g, ':id')
      .replace(/\/\d{3,}/g, '/:id');
    diag.url = (String(location.origin) + path).slice(0, 200);
  } catch (e) {}
  try {
    var s0 = window[STASH];
    diag.stash = s0 ? JSON.stringify(s0.pre) : 'absent';
  } catch (e) {}

  function record(kind, src, result) {
    // NEVER the message text — a confirm prompt can carry PHI.
    try {
      if (diag.calls.length >= 12) diag.calls.shift();
      diag.calls.push(kind + ' via ' + (src || 'NOTHING') + ' -> ' + result);
    } catch (e) {}
  }

  // Bookkeeping must never be able to throw into a dialog call: `diag` is
  // reachable from the page, and a frozen/booby-trapped diag object throwing
  // out of native() would propagate through confirm() — or, worse, out of the
  // install-time probe() and skip the pin() calls below, restoring the
  // original fail-OPEN bug.
  function note(name, tried, label) {
    try {
      diag.tried[name] = tried.join('  ');
      diag.source[name] = label;
    } catch (e) {}
  }

  var cache = {};
  function native(name) {
    if (cache[name]) return cache[name];
    var tried = [];
    for (var i = 0; i < SOURCES.length; i++) {
      var label = SOURCES[i][0];
      var r;
      try { r = SOURCES[i][1](name); } catch (e) { r = { fn: null, why: 'threw' }; }
      tried.push(label + '=' + r.why);
      if (r.fn) {
        note(name, tried, label);
        // The frame realm is re-resolved on every call: the page can rip the
        // iframe out of the DOM, and a detached frame's dialog methods
        // silently become no-ops. Every other source is stable, so cache it.
        if (label !== 'frame') cache[name] = r.fn;
        return r.fn;
      }
    }
    note(name, tried, null);
    return null;
  }

  // ── The globals ───────────────────────────────────────────────────
  function vistaConfirm(message) {
    var fn = native('confirm');
    if (!fn) { record('confirm', null, 'FAIL-CLOSED (returned false)'); report(); return false; }
    var out = false;
    try { out = fn(text(message)) === true; } catch (e) { out = false; }
    record('confirm', diag.source.confirm, String(out));
    return out;
  }

  function vistaAlert(message) {
    var fn = native('alert');
    if (!fn) { record('alert', null, 'FAIL-CLOSED (nothing shown)'); report(); return undefined; }
    try { fn(text(message)); } catch (e) {}
    record('alert', diag.source.alert, 'shown');
    return undefined;
  }

  function vistaPrompt(message, fallback) {
    var fn = native('prompt');
    if (!fn) { record('prompt', null, 'FAIL-CLOSED (returned null)'); report(); return null; }
    var out = null;
    try {
      // Forward a default ONLY when the caller passed one: prompt(msg, undefined)
      // renders the literal string "undefined" in the input box.
      out = arguments.length > 1 ? fn(text(message), text(fallback)) : fn(text(message));
    } catch (e) { out = null; }
    record('prompt', diag.source.prompt, out === null ? 'cancelled' : 'string');
    return out;
  }

  // ── Readable-without-devtools diagnostic ──────────────────────────
  function dump() {
    var out = [];
    out.push('app      : ' + diag.app);
    out.push('url      : ' + diag.url);
    out.push('top frame: ' + diag.top);
    out.push('capture  : ' + diag.stash);
    for (var i = 0; i < NAMES.length; i++) {
      var n = NAMES[i];
      try { if (!diag.tried[n]) native(n); } catch (e) {}
      out.push(n + ' <- ' + (diag.source[n] || 'NOTHING — fails closed'));
      out.push('   tried: ' + diag.tried[n]);
    }
    out.push('calls    :');
    if (!diag.calls.length) { out.push('   (none yet)'); }
    for (var j = 0; j < diag.calls.length; j++) { out.push('   ' + diag.calls[j]); }
    return out.join('\n');
  }

  function button(label, onClick) {
    var b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.style.cssText = 'font:12px/1 Consolas,ui-monospace,monospace;padding:6px 10px;' +
      'border-radius:6px;border:1px solid #55606f;background:#1c232c;color:#e7ecf3;' +
      'cursor:pointer';
    b.addEventListener('click', onClick);
    return b;
  }

  function show(auto) {
    if (!isTop) return;
    try {
      var host = document.body || document.documentElement;
      if (!host) return;
      var old = document.getElementById(OVERLAY_ID);
      if (old) {
        if (auto) return;                                  // already up
        try { old.parentNode.removeChild(old); } catch (e) {}
        return;                                            // hotkey = toggle off
      }
      var box = document.createElement('div');
      box.id = OVERLAY_ID;
      box.style.cssText = 'position:fixed;left:12px;bottom:12px;z-index:2147483647;' +
        'width:560px;max-width:calc(100vw - 24px);max-height:70vh;overflow:auto;' +
        'background:#12161c;color:#e7ecf3;border:1px solid #3a4553;border-radius:10px;' +
        'padding:12px 14px;font:12px/1.5 Consolas,ui-monospace,monospace;' +
        'white-space:pre-wrap;word-break:break-word;box-shadow:0 10px 40px rgba(0,0,0,.55)';

      var head = document.createElement('div');
      head.style.cssText = 'font-weight:700;margin-bottom:8px;color:' +
        (auto ? '#ffb4a2' : '#8bd4ff');
      head.textContent = auto
        ? 'Vista desktop could not open a dialog — the action was BLOCKED, nothing changed.'
        : 'Vista desktop — dialog diagnostics';
      box.appendChild(head);

      var body = document.createElement('div');
      body.textContent = dump();
      box.appendChild(body);

      var row = document.createElement('div');
      row.style.cssText = 'margin-top:10px;display:flex;gap:8px';
      row.appendChild(button('Copy', function () {
        try { navigator.clipboard.writeText(dump()); } catch (e) {}
      }));
      row.appendChild(button('Close', function () {
        try { box.parentNode.removeChild(box); } catch (e) {}
      }));
      box.appendChild(row);

      host.appendChild(box);
    } catch (e) {}
  }

  var autoShown = false;
  function report() {
    if (autoShown || !isTop) return;
    autoShown = true;
    show(true);
  }

  function onKey(e) {
    // Ctrl+Alt+Shift+D. Deliberately obscure; nothing renders otherwise.
    try {
      if (e.ctrlKey && e.altKey && e.shiftKey && (e.key === 'D' || e.key === 'd')) {
        e.preventDefault();
        e.stopPropagation();
        show(false);
      }
    } catch (err) {}
  }

  diag.dump = dump;
  diag.show = function () { show(false); };
  try {
    Object.defineProperty(window, DIAG,
      { value: diag, writable: false, enumerable: false, configurable: true });
  } catch (e) { try { window[DIAG] = diag; } catch (e2) {} }

  // ── Install ───────────────────────────────────────────────────────
  function pin(name, impl) {
    try {
      Object.defineProperty(window, name, {
        configurable: true,
        enumerable: true,
        get: function () { return impl; },
        set: function () { /* swallow the plugin's reassignment; must not throw */ }
      });
    } catch (e) {
      try { window[name] = impl; } catch (e2) {}
    }
  }

  function probe() {
    for (var i = 0; i < NAMES.length; i++) {
      try { if (!diag.source[NAMES[i]]) native(NAMES[i]); } catch (e) {}
    }
  }

  // ⚠️ PROBE BEFORE PIN, ALWAYS. `fromOwn` reads window.confirm's own
  // descriptor; pinning first would replace it with our accessor and
  // permanently blind that source — which is exactly the source that carries
  // us if a future plugin version stops overriding a global (2.7.1 already
  // leaves `prompt` alone). Caught by scenario [6] of the node harness.
  // stash/own/proto all resolve here at document-start; the frame source needs
  // a DOM, so re-probe once the document exists. Failures are never cached, so
  // the second pass really does retry. When the stash works no iframe is ever
  // created — the frame source is not even reached.
  probe();

  pin('confirm', vistaConfirm);
  pin('alert', vistaAlert);
  pin('prompt', vistaPrompt);

  if (isTop) { try { window.addEventListener('keydown', onKey, true); } catch (e) {} }
  try {
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', probe, { once: true });
    }
  } catch (e) {}
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
// Fired by the voice page when it detects a dead/wedged CCP (post-sleep,
// network loss, or Streams' 6-retry ceiling exhausted) and wants a HARD
// reconnect. A plain page reload can't clear the CCP shared worker living in
// the shared WebView2 profile — only recreating the webview does. The page
// only fires this when NO call is active (a re-init mid-call = ghost call), so
// the shell can force-close + reopen the Phone tab safely.
const RECONNECT_SENTINEL: &str = "https://vista-desktop.invalid/__reconnect_phone__";
// Fired by the Platform tab once it knows the signed-in person has phone
// access at this spa (owner 2026-11-18: "the phone tab opens automatically on
// log in from the app and isn't able to be closed"). Followed by
// /<companyId>/<userId>; the shell opens that spa's softphone page as a
// PINNED Phone tab in the background, or replaces a Phone tab that belongs to
// someone else.
const PHONE_ALWAYS_ON_SENTINEL: &str = "https://vista-desktop.invalid/__phone_always_on__";
// Fired by the Platform tab on a deliberate sign-out: the pinned Phone tab
// leaves with the person it belongs to.
const PHONE_SIGNED_OUT_SENTINEL: &str = "https://vista-desktop.invalid/__phone_signed_out__";
// Fired by the Phone tab whenever this computer's call status changes, for
// the status dot on the pinned Phone tab (owner 2026-11-18). Followed by
// available | call | offline | connecting.
const PHONE_STATE_SENTINEL: &str = "https://vista-desktop.invalid/__phone_state__/";
// Fired by the Phone tab when a call starts ringing here (followed by the
// URL-encoded JSON {contactId, name, tag, number}; sent again when the caller
// lookup fills in the name) and when that ring is over (followed by the
// URL-encoded contact id): the incoming-call pop-up (owner 2026-11-18).
const CALL_RINGING_SENTINEL: &str = "https://vista-desktop.invalid/__call_ringing__/";
const CALL_RING_OVER_SENTINEL: &str = "https://vista-desktop.invalid/__call_ring_over__/";
// The Phone or Platform page's LAST resort (v0.3.1): restart the whole app.
// David's Phone tab (2026-11-19) stuck on "Initializing…" for hours because
// the jam lived in this process's WebView2 engine, which rebuilding the tab
// or the page never replaces; only a fresh process did. The shell ends the
// softphone cleanly, waits for the engine to exit, and starts a new copy.
// Refused during a live call. Followed by /<per-launch token>; see
// restart_bridge_script() for why. No UI of its own: the web page decides
// when to call window.__VISTA_RESTART_APP().
const RESTART_APP_SENTINEL: &str = "https://vista-desktop.invalid/__restart_app__/";

// Every bridge sentinel lives under the prefix the Phone-tab recovery (v0.3.3)
// counts as a sign of life and never as a page load. Checked at compile time,
// so a sentinel moved elsewhere cannot silently fall out of that.
const fn has_prefix(s: &str, prefix: &str) -> bool {
    let (s, p) = (s.as_bytes(), prefix.as_bytes());
    if s.len() < p.len() {
        return false;
    }
    let mut i = 0;
    while i < p.len() {
        if s[i] != p[i] {
            return false;
        }
        i += 1;
    }
    true
}
const _: () = {
    let p = phone_recovery::SENTINEL_PREFIX;
    assert!(has_prefix(CLOSE_SENTINEL, p));
    assert!(has_prefix(CALL_SENTINEL, p));
    assert!(has_prefix(AGENT_READY_SENTINEL, p));
    assert!(has_prefix(RECONNECT_SENTINEL, p));
    assert!(has_prefix(PHONE_ALWAYS_ON_SENTINEL, p));
    assert!(has_prefix(PHONE_SIGNED_OUT_SENTINEL, p));
    assert!(has_prefix(PHONE_STATE_SENTINEL, p));
    assert!(has_prefix(CALL_RINGING_SENTINEL, p));
    assert!(has_prefix(CALL_RING_OVER_SENTINEL, p));
    assert!(has_prefix(RESTART_APP_SENTINEL, p));
};

// Label of the incoming-call pop-up window (local popup.html).
const CALL_POPUP: &str = "call-popup";
// The pop-up's size in logical pixels, and its gap from the screen corner.
const POPUP_WIDTH: f64 = 320.0;
const POPUP_HEIGHT: f64 = 150.0;
const POPUP_MARGIN: f64 = 16.0;
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
  window.__VISTA_RECONNECT_PHONE = function () {
    window.open('https://vista-desktop.invalid/__reconnect_phone__');
  };
  window.__VISTA_PHONE_ALWAYS_ON = function (companyId, userId) {
    window.open('https://vista-desktop.invalid/__phone_always_on__/' +
      encodeURIComponent(companyId) + '/' + encodeURIComponent(userId));
  };
  window.__VISTA_PHONE_SIGNED_OUT = function () {
    window.open('https://vista-desktop.invalid/__phone_signed_out__');
  };
  window.__VISTA_PHONE_STATE = function (state) {
    window.open('https://vista-desktop.invalid/__phone_state__/' + encodeURIComponent(state));
  };
  window.__VISTA_CALL_RINGING = function (info) {
    window.open('https://vista-desktop.invalid/__call_ringing__/' + encodeURIComponent(JSON.stringify(info)));
  };
  window.__VISTA_CALL_RING_OVER = function (contactId) {
    window.open('https://vista-desktop.invalid/__call_ring_over__/' + encodeURIComponent(contactId));
  };
})();"#;

/// `window.__VISTA_RESTART_APP()`, defined ONLY in a top-level document on the
/// Platform origin. Bridge scripts are injected into every frame (wry on
/// Windows — see the DIALOG_CAPTURE_SCRIPT notes), so without this any
/// embedded third-party frame could restart the app, and a restart throws
/// away unsaved work in every tab. The sentinel therefore carries a token
/// made fresh for each launch; a frame that isn't top-level Platform returns
/// before it ever touches it, and injected script text is never readable by
/// the page. The shell additionally accepts it only from the Platform tab or
/// a Phone tab whose page is on the Platform origin (decide_new_window).
fn restart_bridge_script() -> String {
    format!(
        "(function () {{\n  if (window.top !== window || location.origin !== 'https://{host}') return;\n  var token = '{token}';\n  window.__VISTA_RESTART_APP = function () {{\n    window.open('{sentinel}' + token);\n  }};\n}})();",
        host = PLATFORM_HOST,
        token = restart_token(),
        sentinel = RESTART_APP_SENTINEL,
    )
}

/// This launch's restart token: 128 bits from std's per-process random
/// SipHash keys (no extra crate). Only the bridge script and the check in
/// decide_new_window ever see it.
fn restart_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        let mut token = String::with_capacity(32);
        for salt in [0x5649_5354_4131_u64, u64::from(std::process::id())] {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(salt);
            token.push_str(&format!("{:016x}", h.finish()));
        }
        token
    })
}

// Managed Connect sign-in windows (Tauri-owned so the shell can close them
// deterministically — the engine-native popup WebView2 spawns for
// window.open could linger as a fully-functional duplicate CCP). Each one
// gets a fresh label, "connect-login-<n>" (v0.3.1): a new sign-in never has
// to wait for, or re-point, a window that is already closing.
const LOGIN_WINDOW: &str = "connect-login";
static LOGIN_COUNTER: AtomicU32 = AtomicU32::new(0);
// The sign-in window that is open, NOT closing and not yet finished, if any.
// Only this one is ever re-pointed at a new sign-in URL: a navigation sent to
// a closing window is lost, and with it the one-time sign-in URL.
static LOGIN_LIVE: Mutex<Option<String>> = Mutex::new(None);
// After the sign-in lands on /voice/signin-complete the shell closes the
// window itself (a page-level window.close() only tears out wry's inner view
// and leaves an empty frame — which also kept the app alive in v0.3.0).
// Normally the page's own window.close() comes first and the shell closes the
// window at once (close_login_on_page_close). These two are fallbacks, and
// they are generous ON PURPOSE: that page's script first notes in shared
// storage that a phone sign-in just completed (platform phoneRecovery,
// 2026-11-19 — how a stuck Phone page knows to stop signing in over and
// over), and on a slow PC it can run well after the page has loaded. Closing
// the window under it would lose that note. GRACE counts from the page's
// load, BACKSTOP from the start of the navigation to it (however the page
// ended). Until then the window just shows "Phone sign-in complete".
const LOGIN_DONE_GRACE_MS: u64 = 3000;
const LOGIN_BACKSTOP_MS: u64 = 10000;

// The Phone page's softphone teardown, run in-page before its web view goes
// away. The page wraps connect.core.terminate() so it also removes the CCP
// frames (platform 9a027dd2); see close_tab_impl for why it must run while
// the tab's own web view is still alive.
const TERMINATE_PHONE_JS: &str = "try{window.connect&&window.connect.core&&window.connect.core.terminate&&window.connect.core.terminate()}catch(e){}";
// How long the terminate gets to reach the CCP's shared worker before the
// web view is destroyed (the same beat close_tab_impl has used since 0.2.x).
const PHONE_TEARDOWN_MS: u64 = 700;

// Quitting for real (v0.3.1). In v0.3.0, once a call had rung, closing the
// main window left vista-platform.exe running invisibly (the hidden call
// pop-up kept it alive, and so did an empty sign-in frame), with no tray icon
// and no single-instance guard: every reopen attached to the same — possibly
// jammed — WebView2 engine. Now closing really quits, and the process outlives
// its engine only until the engine has exited.
static EXITING: AtomicBool = AtomicBool::new(false);
// Start a fresh copy once this one is gone: the restart sentinel, or someone
// opening Vista Platform again while it was still shutting down.
static RELAUNCH: AtomicBool = AtomicBool::new(false);
// finish_exit() has run (it may be reached from the engine-exit event and
// from its fallback timer — only the first counts).
static EXIT_SENT: AtomicBool = AtomicBool::new(false);
// finish_exit() chose request_restart(), so Tauri itself starts the new copy.
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);
// Longest wait for the WebView2 engine to exit once every web view is closed
// (normally well under a second or two). Then the app leaves anyway.
const ENGINE_EXIT_TIMEOUT_MS: u64 = 4000;
// The same for the Phone-tab recovery's automatic restart (v0.3.3): its whole
// point is a fresh engine, and a relaunch that starts against the old one
// would inherit the jam. It only runs at an idle desk, so the wait costs
// nothing (the engine's exit still ends it at once).
const AUTO_RESTART_ENGINE_EXIT_MS: u64 = 20_000;
const AUTO_RESTART_NO_WATCH_MS: u64 = 3_000;

// Whether a call is live right now (armed by the voice page via the call
// sentinel; cleared on end/destroy or when the voice tab is force-closed).
static CALL_ACTIVE: AtomicBool = AtomicBool::new(false);

// The call ringing on this computer right now, as the Phone tab described it
// ({contactId, name, tag, number, startedAt}) — what the pop-up shows.
static RINGING: Mutex<Option<serde_json::Value>> = Mutex::new(None);

type MainJob = Box<dyn FnOnce(&AppHandle) + Send>;

/// Run `job` on the main thread on a LATER turn of the event loop — never
/// inline (v0.3.1 hardening). AppHandle::run_on_main_thread runs the closure
/// immediately when it is called ON the main thread, i.e. from inside
/// whatever WebView2 callback we are in (new-window, navigation, page load,
/// window.close). Creating or destroying windows and web views from inside a
/// WebView2 callback is the documented reentrancy deadlock. One forwarding
/// thread posts the jobs through the event-loop proxy, which is FIFO, so
/// they still run in the order they were queued.
fn later(app: &AppHandle, job: impl FnOnce(&AppHandle) + Send + 'static) {
    static QUEUE: OnceLock<mpsc::Sender<MainJob>> = OnceLock::new();
    let tx = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<MainJob>();
        let app = app.clone();
        std::thread::spawn(move || {
            for job in rx {
                let a = app.clone();
                let _ = app.run_on_main_thread(move || job(&a));
            }
        });
        tx
    });
    if let Err(mpsc::SendError(job)) = tx.send(Box::new(job)) {
        // The forwarding thread is gone: a one-off thread still posts the job
        // (rather than running it inline).
        let app = app.clone();
        std::thread::spawn(move || {
            let a = app.clone();
            let _ = app.run_on_main_thread(move || job(&a));
        });
    }
}

/// `later`, after `ms` milliseconds.
fn later_after(app: &AppHandle, ms: u64, job: impl FnOnce(&AppHandle) + Send + 'static) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        later(&app, job);
    });
}

/// Set the call-active flag AND (Windows) hold/release a system-sleep block so
/// a live call can't be dropped by the machine sleeping. The power request is
/// scheduled onto the MAIN thread so the set (during call) and clear (call end)
/// run on the SAME thread — SetThreadExecutionState's ES_CONTINUOUS state is
/// per-thread, so a clear from a different thread would leave the block stuck
/// on and the machine awake forever. Sleep is only blocked DURING a call; an
/// idle desk (or overnight) still sleeps normally — recovery on wake is the
/// voice page's job.
fn set_call_active(app: &AppHandle, active: bool) {
    CALL_ACTIVE.store(active, Ordering::SeqCst);
    #[cfg(windows)]
    {
        let _ = app.run_on_main_thread(move || {
            use windows::Win32::System::Power::{
                SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
            };
            // SAFETY: a bare Win32 call with no pointers; always on the main thread.
            unsafe {
                let flags = if active {
                    EXECUTION_STATE(ES_CONTINUOUS.0 | ES_SYSTEM_REQUIRED.0)
                } else {
                    ES_CONTINUOUS
                };
                let _ = SetThreadExecutionState(flags);
            }
        });
    }
    #[cfg(not(windows))]
    let _ = app;
}

/// True when a tab URL is the softphone page (/manage/<companyId>/voice).
fn is_voice_url(url: &Url) -> bool {
    url.path().rsplit('/').find(|s| !s.is_empty()) == Some("voice")
}

/// "messaging" / "pos" when `url` is that /manage/<companyId>/<section> page
/// (or below it) — the sections that get one tab each, like Phone.
fn singleton_section(url: &Url) -> Option<&'static str> {
    if url.host_str() != Some(PLATFORM_HOST) {
        return None;
    }
    let mut segs = url.path().split('/').filter(|s| !s.is_empty());
    if segs.next() != Some("manage") {
        return None;
    }
    segs.next()?; // companyId
    match segs.next()? {
        "messaging" => Some("messaging"),
        "pos" => Some("pos"),
        _ => None,
    }
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

/// The pre-plugin dialog capture, packaged as a Tauri plugin purely to get at
/// the plugin init-script slot — it registers no commands, no setup hook and
/// no config, so it can never fail to initialize (TauriPlugin::initialize only
/// deserializes `plugins.<name>` when a setup hook exists; ours has none).
///
/// MUST be registered before `tauri_plugin_dialog::init()`: plugin init
/// scripts are emitted in registration order (see the DIALOG_CAPTURE_SCRIPT
/// comment block), and this one has to see pristine natives.
///
/// Because it is a PLUGIN script rather than a builder script, it reaches
/// EVERY webview automatically — content tabs, the title bar, and the managed
/// Connect sign-in window — with no per-site wiring and no change to any
/// existing `format!` arity.
fn dialog_capture_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new(DIALOG_CAPTURE_PLUGIN)
        .js_init_script(DIALOG_CAPTURE_SCRIPT)
        .build()
}

/// Marks the page as running inside the desktop app. The web app reads
/// `window.__VISTA_DESKTOP__` to suppress its "install the desktop app"
/// prompt. Injected at document start into every content/tab webview.
fn desktop_marker_script() -> String {
    format!(
        "window.__VISTA_DESKTOP__ = '{}';",
        env!("CARGO_PKG_VERSION")
    )
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
    /// Labels of PINNED tabs (the always-on Phone tab): no ×, right after
    /// Platform, closed only by the shell itself (sign-out, a hard reconnect,
    /// a different person's phone, quitting the app).
    pinned: Vec<String>,
    /// "<companyId>/<userId>" whose phone the pinned Phone tab is, so the
    /// next person to sign in on this computer never inherits it.
    phone_owner: Option<String>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Let the CCP ringtone + remote-call audio play without a user gesture.
    // A parked/off-screen phone webview never gets a gesture, so Chromium's
    // autoplay policy would keep the AudioContext suspended → no ring, no
    // inbound audio. Must be set before any WebView2 environment is created.
    // (Belt-and-suspenders with the page's silent-AudioContext keep-alive.)
    #[cfg(windows)]
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "--autoplay-policy=no-user-gesture-required",
    );
    tauri::Builder::default()
        // Single instance (v0.3.1) — FIRST, as the plugin requires: a second
        // launch must hand over and exit while plugins initialize, before any
        // other plugin or window starts. It injects no init script, so the
        // dialog capture below is still the first SCRIPT to run.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            on_second_launch(app)
        }))
        // ⚠️ ORDER IS LOAD-BEARING. This must stay the first plugin WITH AN
        // INIT SCRIPT, and before tauri_plugin_dialog::init(): plugin init
        // scripts run in registration order, and the capture only works while
        // window.confirm/alert/prompt are still the pristine natives — i.e.
        // strictly before the dialog plugin injects its init-iife.js. Moving
        // it below the dialog plugin re-breaks every confirm-gated destructive
        // action in the shell.
        .plugin(dialog_capture_plugin())
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
                //
                // DIALOG_CAPTURE_SCRIPT is NOT listed here — it ships as a
                // plugin script (dialog_capture_plugin) and is prepended to
                // every webview automatically, ahead of the dialog plugin's.
                // desktop_marker_script() must stay FIRST: the dialog
                // diagnostic reads window.__VISTA_DESKTOP__ for the version.
                // 4 placeholders, 4 args.
                .initialization_script(format!(
                    "{}{}{}{}",
                    desktop_marker_script(),
                    DIALOG_GLOBALS_SCRIPT,
                    VOICE_BRIDGE_SCRIPT,
                    restart_bridge_script()
                ))
                .on_new_window(move |url, _features| decide_new_window(&nw_app, PLATFORM_TAB, url))
                .on_navigation(move |nav_url| {
                    if let Some(flag) = nav_url.as_str().strip_prefix(CALL_SENTINEL) {
                        set_call_active(&nav_plat_app, flag.starts_with('1'));
                        return false; // cancel the sentinel nav; page stays put
                    }
                    if nav_url.as_str().starts_with(AGENT_READY_SENTINEL) {
                        later(&nav_plat_app, close_login_window);
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
                pinned: Vec::new(),
                phone_owner: None,
            }));

            // Keep the title bar + the active tab sized to the window; block
            // a window close while a call is live (closing hangs up with no
            // rejoin — the title bar shows Keep call / Close anyway instead);
            // otherwise closing the window QUITS the app (v0.3.1, owner:
            // "closing the app should really close it").
            let resize_app = app.handle().clone();
            window.on_window_event(move |event| {
                match event {
                    WindowEvent::Resized(_) => relayout(&resize_app),
                    // VISTA came to the front or went behind / minimized:
                    // the incoming-call pop-up follows (owner 2026-11-18).
                    WindowEvent::Focused(_) => refresh_popup(&resize_app),
                    WindowEvent::CloseRequested { api, .. } => {
                        // The window never closes on its own: the Phone tab's
                        // softphone must be ended while its web view is still
                        // alive, so begin_exit tears down first and then
                        // exits the whole app (taking this window with it).
                        api.prevent_close();
                        if EXITING.load(Ordering::SeqCst) {
                            // Already on the way out.
                        } else if CALL_ACTIVE.load(Ordering::SeqCst) {
                            let _ = resize_app.emit(
                                "call:close-blocked",
                                serde_json::json!({ "kind": "window" }),
                            );
                        } else {
                            later(&resize_app, |app| begin_exit(app, false));
                        }
                    }
                    WindowEvent::Destroyed => main_window_gone(&resize_app),
                    _ => {}
                }
            });
            // Initial layout once the window is on-screen (the OS may
            // adjust the size before the webviews attach).
            relayout(app.handle());
            // Phone-tab self-recovery (v0.3.3): its watchdog and log.
            phone_recovery::start(app.handle());

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
            popup_current,
            popup_ready,
            popup_answer,
            popup_ignore,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Someone opened Vista Platform again while this copy was already
            // shutting down, after it had chosen a plain exit: start the new
            // copy now. The single-instance plugin has just released its lock
            // (plugins see Exit before this callback), so the new copy becomes
            // the running one instead of handing over to this dying one.
            if let RunEvent::Exit = event {
                if RELAUNCH.load(Ordering::SeqCst) && !RESTART_REQUESTED.load(Ordering::SeqCst) {
                    app.cleanup_before_exit();
                    tauri::process::restart(&app.env());
                }
            }
        });
}

// ─── Quitting and restarting (v0.3.1) ───────────────────────────────

/// A second launch of Vista Platform (single-instance plugin; runs on the main
/// thread). Normally: bring the running window to the front. While this copy
/// is shutting down: remember to start a fresh copy once it is gone, so the
/// launch isn't lost.
fn on_second_launch(app: &AppHandle) {
    if EXITING.load(Ordering::SeqCst) {
        RELAUNCH.store(true, Ordering::SeqCst);
        return;
    }
    if let Some(main) = app.get_window("main") {
        let _ = main.unminimize();
        let _ = main.show();
        let _ = main.set_focus();
    }
}

/// Safety net: nothing may outlive the main window. (Only the exit itself
/// destroys it today; anything else that ever does takes the app with it.)
fn main_window_gone(app: &AppHandle) {
    if !EXITING.swap(true, Ordering::SeqCst) {
        app.exit(0);
    }
}

/// Leave the app for real — `relaunch` starts a fresh copy afterwards. In
/// order: every Phone tab ends its softphone in-page (the same wrapped
/// connect.core.terminate() close_tab_impl runs) while its web view is still
/// alive; nothing stays on screen (main window hidden, call pop-up and sign-in
/// windows closed); after the teardown beat every web view is closed, the
/// WebView2 engine is given time to exit, and only then does the process exit
/// (finish_exit). A relaunch therefore always gets a fresh engine. Callers
/// check the live-call guard first ("Close anyway" deliberately skips it).
/// Runs on the main thread.
fn begin_exit(app: &AppHandle, relaunch: bool) {
    if relaunch {
        RELAUNCH.store(true, Ordering::SeqCst);
    }
    if EXITING.swap(true, Ordering::SeqCst) {
        return;
    }
    set_call_active(app, false);
    *RINGING.lock().unwrap() = None;
    let phones = app.state::<Mutex<TabState>>().lock().unwrap().voice.clone();
    let mut any_phone = false;
    for label in &phones {
        if let Some(wv) = app.get_webview(label) {
            let _ = wv.eval(TERMINATE_PHONE_JS);
            any_phone = true;
        }
    }
    if let Some(main) = app.get_window("main") {
        let _ = main.hide();
    }
    if let Some(popup) = app.get_webview_window(CALL_POPUP) {
        let _ = popup.close();
    }
    close_login_window(app);
    let beat = if any_phone { PHONE_TEARDOWN_MS } else { 0 };
    later_after(app, beat, close_all_then_exit);
}

/// Close every web view so the WebView2 engine shuts down, then exit once it
/// has (or after ENGINE_EXIT_TIMEOUT_MS; AUTO_RESTART_ENGINE_EXIT_MS for the
/// Phone-tab recovery's automatic restart). The main window stays, hidden,
/// until the process exits, so Tauri doesn't quit early on "last window
/// closed". Runs on the main thread.
fn close_all_then_exit(app: &AppHandle) {
    #[cfg(windows)]
    let watching = watch_engine_exit(app);
    #[cfg(not(windows))]
    let watching = false;
    // Windows first: webview_windows() only lists windows whose web view is
    // still open.
    for (label, window) in app.webview_windows() {
        if label != "main" {
            let _ = window.close();
        }
    }
    for webview in app.webviews().into_values() {
        let _ = webview.close();
    }
    let wait = match (watching, phone_recovery::restarting()) {
        (true, true) => AUTO_RESTART_ENGINE_EXIT_MS,
        (true, false) => ENGINE_EXIT_TIMEOUT_MS,
        (false, true) => AUTO_RESTART_NO_WATCH_MS,
        (false, false) => 300,
    };
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(wait));
        // Which way this copy left, when it restarted itself (v0.3.3 log).
        phone_recovery::note_exit(if watching {
            "engine-timeout"
        } else {
            "no-engine-watch"
        });
        finish_exit(&app);
    });
}

/// The last step of begin_exit (once; any thread). Both paths fire
/// RunEvent::Exit, where the single-instance plugin releases its lock BEFORE
/// any new copy starts. (AppHandle::restart() called on the main thread skips
/// that event, so the new copy would find the lock still held, hand over to
/// this dying copy and exit — leaving nothing running.)
fn finish_exit(app: &AppHandle) {
    if EXIT_SENT.swap(true, Ordering::SeqCst) {
        return;
    }
    if RELAUNCH.load(Ordering::SeqCst) {
        RESTART_REQUESTED.store(true, Ordering::SeqCst);
        app.request_restart();
    } else {
        app.exit(0);
    }
}

/// Have finish_exit run the moment this app's WebView2 browser process exits
/// (after every web view is closed). Returns false when the engine can't be
/// watched; the caller then just waits a moment. The engine is shared by
/// every web view here (same profile, same options), and with the
/// single-instance plugin no other copy of the app can be using it. Runs on
/// the main thread, BEFORE the web views are closed.
#[cfg(windows)]
fn watch_engine_exit(app: &AppHandle) -> bool {
    let Some(webview) = app
        .get_webview(PLATFORM_TAB)
        .or_else(|| app.webviews().into_values().next())
    else {
        return false;
    };
    let (tx, rx) = mpsc::channel::<bool>();
    let done_app = app.clone();
    let result = webview.with_webview(move |platform_webview| {
        let watched = on_engine_exit(&platform_webview.controller(), done_app);
        if let Err(e) = &watched {
            eprintln!("[vista-desktop] can't watch the engine exit: {e}");
        }
        let _ = tx.send(watched.is_ok());
    });
    // with_webview runs inline on the main thread, so the answer is already
    // there. Never block waiting for it: if it were queued instead, waiting
    // here would hold up the very loop that has to run it.
    result.is_ok() && rx.try_recv().unwrap_or(false)
}

/// Register finish_exit on the BrowserProcessExited event of `controller`'s
/// WebView2 environment. Call on the UI thread that owns the controller.
#[cfg(windows)]
fn on_engine_exit(
    controller: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
    done_app: AppHandle,
) -> windows::core::Result<()> {
    use webview2_com::BrowserProcessExitedEventHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Environment5, ICoreWebView2_2,
    };
    use windows::core::Interface;

    // SAFETY: WebView2 COM calls on the UI thread that owns the controller
    // (with_webview runs us there); the event handler fires on it too.
    unsafe {
        let env = controller
            .CoreWebView2()?
            .cast::<ICoreWebView2_2>()?
            .Environment()?
            .cast::<ICoreWebView2Environment5>()?;
        let handler = BrowserProcessExitedEventHandler::create(Box::new(move |_, _| {
            phone_recovery::note_exit("engine-exited");
            finish_exit(&done_app);
            Ok(())
        }));
        let mut token: i64 = 0;
        env.add_BrowserProcessExited(&handler, &mut token)?;
        // Keep the environment (and so the registration) alive until the
        // event fires; the process is about to exit anyway.
        std::mem::forget(env);
    }
    Ok(())
}

/// The restart sentinel: accepted only from the Platform tab or a Phone tab
/// whose page is on the Platform origin, carrying this launch's token, and
/// never during a live call.
fn restart_allowed(app: &AppHandle, from: &str, token: &str) -> bool {
    if token != restart_token() || CALL_ACTIVE.load(Ordering::SeqCst) {
        return false;
    }
    let phone_or_platform = from == PLATFORM_TAB || {
        let s = app.state::<Mutex<TabState>>();
        let s = s.lock().unwrap();
        s.voice.iter().any(|v| v == from)
    };
    phone_or_platform
        && app
            .get_webview(from)
            .and_then(|wv| wv.url().ok())
            .is_some_and(|url| url.scheme() == "https" && url.host_str() == Some(PLATFORM_HOST))
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
        let Ok(updater) = handle.updater() else {
            return;
        };
        let Ok(Some(update)) = updater.check().await else {
            return;
        };
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
                    // On Windows the installer takes over: download_and_install
                    // launches it and ends this process itself (so the
                    // single-instance lock goes with it) and the installer
                    // reopens the new version. Elsewhere it returns, and
                    // request_restart goes through RunEvent::Exit, where the
                    // single-instance plugin lets go before the new copy starts.
                    if update.download_and_install(|_, _| {}, || {}).await.is_ok() {
                        restart_handle.request_restart();
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
/// else opens in the system browser. `from` is the label of the tab whose
/// page asked.
///
/// Window and web view work is queued with `later` (v0.3.1): wry already runs
/// this after the WebView2 callback returns, but the request's deferral is
/// still open until we return; nothing is created or destroyed under it.
fn decide_new_window(app: &AppHandle, from: &str, url: Url) -> NewWindowResponse<tauri::Wry> {
    // The app is quitting: open nothing, start nothing.
    if EXITING.load(Ordering::SeqCst) {
        return NewWindowResponse::Deny;
    }
    // Any bridge sentinel proves the page that sent it is running: the
    // Phone-tab watchdog counts it as a sign of life (v0.3.3).
    if url.as_str().starts_with(phone_recovery::SENTINEL_PREFIX) {
        phone_recovery::page_alive(from);
    }
    // Voice-bridge sentinels arrive as window.open() (see VOICE_BRIDGE_SCRIPT
    // beforeunload note) — consume them here, open nothing.
    if let Some(flag) = url.as_str().strip_prefix(CALL_SENTINEL) {
        set_call_active(app, flag.starts_with('1'));
        return NewWindowResponse::Deny;
    }
    if url.as_str().starts_with(AGENT_READY_SENTINEL) {
        later(app, close_login_window);
        return NewWindowResponse::Deny;
    }
    if url.as_str().starts_with(RECONNECT_SENTINEL) {
        // Who asked matters: it force-closes the Phone tab (v0.3.3).
        let from = from.to_string();
        later(app, move |app| reconnect_voice(app, &from));
        return NewWindowResponse::Deny;
    }
    if let Some(token) = url.as_str().strip_prefix(RESTART_APP_SENTINEL) {
        if restart_allowed(app, from, token) {
            later(app, |app| {
                // Re-checked here: a call may have started in between.
                if !CALL_ACTIVE.load(Ordering::SeqCst) {
                    begin_exit(app, true);
                }
            });
        }
        return NewWindowResponse::Deny;
    }
    if let Some(rest) = url.as_str().strip_prefix(PHONE_ALWAYS_ON_SENTINEL) {
        if let Some((company, user)) = parse_phone_owner(rest) {
            later(app, move |app| ensure_phone_tab(app, company, user));
        }
        return NewWindowResponse::Deny;
    }
    if url.as_str().starts_with(PHONE_SIGNED_OUT_SENTINEL) {
        later(app, close_phone_tabs);
        return NewWindowResponse::Deny;
    }
    if let Some(state) = url.as_str().strip_prefix(PHONE_STATE_SENTINEL) {
        let state = percent_decode(state);
        if matches!(
            state.as_str(),
            "available" | "call" | "offline" | "connecting"
        ) {
            let _ = app.emit("phone:state", serde_json::json!({ "state": state }));
            // A softphone that reports ready cancels a pending automatic
            // restart (v0.3.3).
            phone_recovery::page_state(from, &state);
        }
        return NewWindowResponse::Deny;
    }
    if let Some(payload) = url.as_str().strip_prefix(CALL_RINGING_SENTINEL) {
        if let Some(info) = parse_ringing(&percent_decode(payload)) {
            *RINGING.lock().unwrap() = Some(info);
            later(app, refresh_popup);
        }
        return NewWindowResponse::Deny;
    }
    if let Some(id) = url.as_str().strip_prefix(CALL_RING_OVER_SENTINEL) {
        let id = percent_decode(id);
        let cleared = {
            let mut ringing = RINGING.lock().unwrap();
            let same = ringing
                .as_ref()
                .and_then(|r| r.get("contactId"))
                .and_then(|v| v.as_str())
                .is_some_and(|cur| cur == id);
            if same {
                *ringing = None;
            }
            same
        };
        if cleared {
            later(app, refresh_popup);
        }
        return NewWindowResponse::Deny;
    }
    if is_auth_popup_host(url.host_str().unwrap_or("")) {
        later(app, move |app| open_login_window(app, url));
        return NewWindowResponse::Deny;
    }
    route_new_window(app, url);
    NewWindowResponse::Deny
}

/// Open (or focus) the managed Connect sign-in window. Closed by the
/// agent-ready sentinel once the softphone session is live, so it can only
/// ever exist while a sign-in is genuinely in progress.
///
/// DELIBERATELY gets no `initialization_script`: this window loads AWS/Cognito
/// identity-provider pages, and we do not inject our diagnostic overlay or a
/// keydown hook into a third party's sign-in form. Re-confirmed 2026-08-11 —
/// and it is now moot for the actual bug, because DIALOG_CAPTURE_SCRIPT is a
/// PLUGIN script and therefore already runs here (as it does in `title_bar`).
/// So this window's confirm/alert are pinned to the real natives too; only the
/// layered fallbacks and the diagnostic UI are absent. Nothing here is
/// confirm-gated anyway.
///
/// An EXISTING window is re-pointed at the new URL (2026-10-28): the sign-in
/// URL is one-time, and the old "just focus it" left a stalled, blank window
/// swallowing every later sign-in until the app was restarted — a page
/// refresh or "Reconnect now" could never get past "Initializing…". Only the
/// LIVE window is re-pointed, never one that is closing (v0.3.1): that one is
/// left to close and the sign-in gets a fresh window.
///
/// Once the sign-in lands on /voice/signin-complete the shell closes the
/// window itself (v0.3.1): at once when the page calls window.close() itself
/// (the normal case — that otherwise destroys only wry's inner view and
/// leaves an empty frame on screen), else a few seconds after that page has
/// loaded (time for its script to note the completed sign-in), and, failing
/// both, a few seconds after it landed. Runs on the main thread.
fn open_login_window(app: &AppHandle, url: Url) {
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    let live = LOGIN_LIVE.lock().unwrap().clone();
    if let Some(label) = live {
        if let Some(existing) = app.get_webview_window(&label) {
            match existing.navigate(url.clone()) {
                Ok(()) => {
                    let _ = existing.set_focus();
                    return;
                }
                Err(e) => {
                    eprintln!("[vista-desktop] login window navigate failed: {e}");
                    close_login_label(app, &label);
                }
            }
        }
    }
    let label = format!(
        "{LOGIN_WINDOW}-{}",
        LOGIN_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let nav_app = app.clone();
    let nav_label = label.clone();
    let load_app = app.clone();
    let result = tauri::WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url))
        .title("Sign in — Vista Phone")
        .inner_size(480.0, 700.0)
        .center()
        .on_navigation(move |nav_url| {
            if is_signin_complete(nav_url) {
                // This window's sign-in is done: a later sign-in gets a fresh
                // window rather than this one, so the closes queued for it
                // can never cut a new sign-in short.
                retire_login_label(&nav_label);
                // Backstop: gone a few seconds after the sign-in landed,
                // however the page ends.
                let label = nav_label.clone();
                later_after(&nav_app, LOGIN_BACKSTOP_MS, move |app| {
                    close_login_label(app, &label)
                });
            }
            true
        })
        .on_page_load(move |window, payload| {
            if payload.event() == PageLoadEvent::Finished && is_signin_complete(payload.url()) {
                let label = window.label().to_string();
                later_after(&load_app, LOGIN_DONE_GRACE_MS, move |app| {
                    close_login_label(app, &label)
                });
            }
        })
        .build();
    let window = match result {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[vista-desktop] failed to open login window: {e}");
            return;
        }
    };
    *LOGIN_LIVE.lock().unwrap() = Some(label.clone());
    // Closed from its own title bar, or by the shell: no longer the live one.
    let event_label = label.clone();
    window.on_window_event(move |event| {
        if matches!(
            event,
            WindowEvent::CloseRequested { .. } | WindowEvent::Destroyed
        ) {
            retire_login_label(&event_label);
        }
    });
    #[cfg(windows)]
    close_login_on_page_close(app, &label);
}

/// The page the federation sign-in lands on once AWS has set the Connect
/// session cookies (platform app/voice/signin-complete).
fn is_signin_complete(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some(PLATFORM_HOST)
        && url.path().trim_end_matches('/') == "/voice/signin-complete"
}

/// Close every managed sign-in window (agent session is live → login done;
/// a hard reconnect; quitting).
fn close_login_window(app: &AppHandle) {
    *LOGIN_LIVE.lock().unwrap() = None;
    for (label, window) in app.webview_windows() {
        if label.starts_with(LOGIN_WINDOW) {
            let _ = window.close();
        }
    }
}

/// Close one sign-in window (the one that just finished, or failed).
fn close_login_label(app: &AppHandle, label: &str) {
    retire_login_label(label);
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.close();
    }
}

/// `label` is no longer the sign-in window a new sign-in may re-point.
fn retire_login_label(label: &str) {
    let mut live = LOGIN_LIVE.lock().unwrap();
    if live.as_deref() == Some(label) {
        *live = None;
    }
}

/// When a page in the sign-in window calls window.close(), close the whole
/// window. wry's own handler for that event destroys only its inner view and
/// leaves the empty frame on screen (and, in v0.3.0, the app alive). Both
/// handlers run; ours just queues the real close.
#[cfg(windows)]
fn close_login_on_page_close(app: &AppHandle, label: &str) {
    use webview2_com::WindowCloseRequestedEventHandler;

    let Some(window) = app.get_webview_window(label) else {
        return;
    };
    let close_app = app.clone();
    let close_label = label.to_string();
    let result = window.with_webview(move |platform_webview| {
        // SAFETY: WebView2 COM calls on the UI thread that with_webview runs
        // on; the handler fires on that thread too and only queues work.
        unsafe {
            let core = match platform_webview.controller().CoreWebView2() {
                Ok(core) => core,
                Err(e) => {
                    eprintln!("[vista-desktop] login window CoreWebView2() failed: {e}");
                    return;
                }
            };
            let handler = WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
                let label = close_label.clone();
                later(&close_app, move |app| close_login_label(app, &label));
                Ok(())
            }));
            let mut token: i64 = 0;
            if let Err(e) = core.add_WindowCloseRequested(&handler, &mut token) {
                eprintln!("[vista-desktop] login window close hook failed: {e}");
            }
        }
    });
    if let Err(e) = result {
        eprintln!("[vista-desktop] login window with_webview failed: {e}");
    }
}

// ─── Always-on Phone tab + incoming-call pop-up (owner 2026-11-18) ──
//
// Mockup https://claude.ai/artifact/1u6edSf3cPkBbsBXjaTLHh, approved with:
// only people with phone access get the pinned tab (the web app decides and
// sends PHONE_ALWAYS_ON); a status dot on it; a pop-up in the screen corner
// while a call rings here and the Phone tab isn't in front, whose Ignore
// passes the call on (the page's Decline path).

/// Decode %XX escapes as UTF-8. Sentinel payloads are encodeURIComponent'd by
/// the page; a malformed escape is kept as written.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let byte = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = byte {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A VISTA id as the web app writes it: 36 characters of hex digits and dashes
/// (one rule, shared with the Phone-tab recovery).
fn is_vista_id(s: &str) -> bool {
    phone_recovery::is_vista_id(s)
}

/// "/<companyId>/<userId>" (each URL-encoded) → the two ids, lower-cased.
fn parse_phone_owner(rest: &str) -> Option<(String, String)> {
    let mut parts = rest
        .split('/')
        .filter(|p| !p.is_empty())
        .map(percent_decode);
    let company = parts.next()?;
    let user = parts.next()?;
    if parts.next().is_some() || !is_vista_id(&company) || !is_vista_id(&user) {
        return None;
    }
    Some((company.to_ascii_lowercase(), user.to_ascii_lowercase()))
}

/// The Phone tab's ringing payload, trimmed to what the pop-up shows.
fn parse_ringing(json: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let contact_id = v.get("contactId")?.as_str()?.trim().to_string();
    if contact_id.is_empty() || contact_id.len() > 64 {
        return None;
    }
    let text = |key: &str, max: usize| -> serde_json::Value {
        match v.get(key).and_then(|x| x.as_str()).map(str::trim) {
            Some(s) if !s.is_empty() => serde_json::Value::String(s.chars().take(max).collect()),
            _ => serde_json::Value::Null,
        }
    };
    let started_at = v.get("startedAt").and_then(|x| x.as_f64()).unwrap_or(0.0);
    Some(serde_json::json!({
        "contactId": contact_id,
        "name": text("name", 80),
        "tag": text("tag", 24),
        "number": text("number", 32),
        "startedAt": started_at,
    }))
}

/// Open (or keep) the pinned, always-on Phone tab for `company`/`user`. It
/// opens in the background (the person stays where they are), right after
/// Platform, with no ×. A Phone tab opened earlier from the sidebar for the
/// same spa is adopted as the pinned one; a Phone tab that belongs to someone
/// else (another person signed in on this computer, or another spa) is
/// replaced, never during a live call (the next page load asks again). Runs
/// on the main thread.
fn ensure_phone_tab(app: &AppHandle, company: String, user: String) {
    let owner = format!("{company}/{user}");
    let Ok(url) = format!("https://{PLATFORM_HOST}/manage/{company}/voice").parse::<Url>() else {
        return;
    };
    // A rebuild of this person's Phone tab is already on its way (v0.3.3).
    if !phone_recovery::on_ensure(app, &owner) {
        return;
    }
    let (existing, current_owner) = {
        let s = app.state::<Mutex<TabState>>();
        let s = s.lock().unwrap();
        (s.voice.first().cloned(), s.phone_owner.clone())
    };
    let Some(label) = existing else {
        open_tab_ex(app, url, true, Some(owner));
        return;
    };
    let spa_prefix = format!("/manage/{company}/");
    let same_spa = app
        .get_webview(&label)
        .and_then(|wv| wv.url().ok())
        .is_some_and(|cur| cur.path().to_ascii_lowercase().starts_with(&spa_prefix));
    let same_person = current_owner.as_deref().is_none_or(|o| o == owner);
    if same_spa && same_person {
        pin_tab(app, &label, owner);
        return;
    }
    if CALL_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    close_tab_impl(app, &label, true);
    // Let the old phone's teardown (frame removal, then the deferred webview
    // drop) finish before the new one starts, as reconnect_voice does.
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let a = app2.clone();
        let _ = app2.run_on_main_thread(move || open_tab_ex(&a, url, true, Some(owner)));
    });
}

/// Make `label` the pinned Phone tab: right after Platform, no ×, owned by
/// `owner`. Tells the title bar. Runs on the main thread.
fn pin_tab(app: &AppHandle, label: &str, owner: String) {
    let index = {
        let s = app.state::<Mutex<TabState>>();
        let mut s = s.lock().unwrap();
        if !s.pinned.iter().any(|l| l == label) {
            s.pinned.push(label.to_string());
        }
        s.phone_owner = Some(owner);
        if let Some(i) = s.order.iter().position(|l| l == label) {
            let moved = s.order.remove(i);
            let at = 1.min(s.order.len());
            s.order.insert(at, moved);
        }
        s.order.iter().position(|l| l == label).unwrap_or(1)
    };
    let _ = app.emit(
        "tab:pinned",
        serde_json::json!({ "label": label, "index": index }),
    );
}

/// Sign-out: close every Phone tab (the pinned one leaves with its person)
/// and the pop-up. A live call is never hung up: the tab stays until it ends,
/// and the next person's sign-in replaces it then. Runs on the main thread.
fn close_phone_tabs(app: &AppHandle) {
    // Stop the Phone-tab watchdog first, even if a live call keeps the tab.
    phone_recovery::on_signed_out(app);
    if CALL_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let labels = {
        let s = app.state::<Mutex<TabState>>();
        let mut s = s.lock().unwrap();
        s.phone_owner = None;
        s.voice.clone()
    };
    for label in labels {
        close_tab_impl(app, &label, true);
    }
    *RINGING.lock().unwrap() = None;
    hide_popup(app);
}

/// Show, update or hide the incoming-call pop-up to match what rings here.
/// Shown while a call rings unless VISTA is in front with the Phone tab
/// showing (its own caller card is enough then). Runs on the main thread.
fn refresh_popup(app: &AppHandle) {
    let ringing = RINGING.lock().unwrap().clone();
    let Some(info) = ringing.filter(|_| !EXITING.load(Ordering::SeqCst)) else {
        hide_popup(app);
        return;
    };
    let phone_tab_active = {
        let s = app.state::<Mutex<TabState>>();
        let s = s.lock().unwrap();
        s.voice.iter().any(|v| *v == s.active)
    };
    let window_in_front = app
        .get_window("main")
        .is_some_and(|w| w.is_focused().unwrap_or(false) && !w.is_minimized().unwrap_or(false));
    if phone_tab_active && window_in_front {
        hide_popup(app);
    } else {
        show_popup(app, info);
    }
}

/// Fill and show the pop-up, creating it on first use. A new window stays
/// hidden until popup.html has drawn the call (popup_ready), so no empty card
/// flashes up. It can never take focus: someone typing keeps typing.
fn show_popup(app: &AppHandle, info: serde_json::Value) {
    if let Some(w) = app.get_webview_window(CALL_POPUP) {
        let _ = app.emit_to(CALL_POPUP, "popup:show", info);
        place_popup(app, &w);
        let _ = w.show();
        return;
    }
    // Created on a later turn (refresh_popup is also reached from window
    // events, e.g. Focused), never in the middle of another callback.
    later(app, create_popup);
}

/// Create the (hidden) pop-up window if a call still rings and it doesn't
/// exist yet; popup.html then asks for the call and popup_ready shows it.
/// Runs on the main thread.
fn create_popup(app: &AppHandle) {
    let still_ringing = RINGING.lock().unwrap().is_some();
    if !still_ringing
        || EXITING.load(Ordering::SeqCst)
        || app.get_webview_window(CALL_POPUP).is_some()
    {
        return;
    }
    let built =
        tauri::WebviewWindowBuilder::new(app, CALL_POPUP, WebviewUrl::App("popup.html".into()))
            .title("Incoming call")
            .inner_size(POPUP_WIDTH, POPUP_HEIGHT)
            .decorations(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .focusable(false)
            .visible(false)
            .shadow(true)
            .build();
    match built {
        Ok(w) => place_popup(app, &w),
        Err(e) => eprintln!("[vista-desktop] failed to open the call pop-up: {e}"),
    }
}

/// Bottom-right corner of the screen VISTA is on, above the taskbar.
fn place_popup(app: &AppHandle, popup: &tauri::WebviewWindow) {
    let monitor = app
        .get_window("main")
        .and_then(|w| w.current_monitor().ok().flatten())
        .or_else(|| popup.primary_monitor().ok().flatten());
    let Some(m) = monitor else { return };
    let scale = m.scale_factor();
    let area = m.work_area();
    let w = (POPUP_WIDTH * scale).round() as i32;
    let h = (POPUP_HEIGHT * scale).round() as i32;
    let margin = (POPUP_MARGIN * scale).round() as i32;
    let x = area.position.x + area.size.width as i32 - w - margin;
    let y = area.position.y + area.size.height as i32 - h - margin;
    let _ = popup.set_position(tauri::PhysicalPosition::new(x, y));
}

fn hide_popup(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(CALL_POPUP) {
        let _ = w.hide();
    }
}

/// Call one of the Phone page's hooks with a contact id (JSON-quoted, so the
/// id can never break out of the string).
fn call_phone_page(app: &AppHandle, label: &str, hook: &str, contact_id: &str) {
    let Some(wv) = app.get_webview(label) else {
        return;
    };
    let arg = serde_json::Value::String(contact_id.to_string()).to_string();
    let script = format!("try{{window.{hook}&&window.{hook}({arg})}}catch(e){{}}");
    let _ = wv.eval(&script);
}

/// The first Phone tab's label, if one is open.
fn phone_tab_label(app: &AppHandle) -> Option<String> {
    let s = app.state::<Mutex<TabState>>();
    let s = s.lock().unwrap();
    s.voice.first().cloned()
}

/// Open an internal /manage/* page as a tab, anything else in the system
/// browser. Window/webview mutations are deferred to a later main-thread turn
/// to avoid reentrancy while the webview engine is mid new-window-request.
fn route_new_window(app: &AppHandle, url: Url) {
    let is_internal_page =
        url.host_str() == Some(PLATFORM_HOST) && url.path().starts_with("/manage/");
    if is_internal_page {
        later(app, move |app| open_tab(app, url));
    } else if let Err(e) = open::that_detached(url.as_str()) {
        eprintln!("[vista-desktop] failed to open {url} in browser: {e}");
    }
}

/// Hard-reconnect the softphone at the page's request (the reconnect
/// sentinel, sent by tab `from`): force-close the (single) Phone tab and
/// reopen a fresh one. The voice page only fires the sentinel when NO call is
/// live, so force-closing is safe; recreating the webview clears the wedged
/// CCP shared worker a page reload can't. Since v0.3.3 this goes through the
/// Phone-tab recovery: only a request from the Phone tab itself counts (the
/// bridge runs in every tab and frame), one rebuild in flight at a time, the
/// pinned tab rebuilt from its owner's /manage/<company>/voice (never from
/// about:blank), in the background unless the person was looking at it, after
/// closing a stalled sign-in window; a request that arrives while a rebuild is
/// pending is ignored, and none is rebuilt for a person who signed out. Runs
/// on the main thread.
fn reconnect_voice(app: &AppHandle, from: &str) {
    phone_recovery::page_asked_reconnect(app, from);
}

/// Add a new content webview as a tab, make it active, and tell the
/// title bar to render it. Runs on the main thread.
fn open_tab(app: &AppHandle, url: Url) {
    open_tab_ex(app, url, false, None);
}

/// `open_tab`, plus: `background` keeps the person on the tab they're on (the
/// new tab is laid out behind it), and `pin_owner` makes the tab the pinned,
/// always-on Phone tab owned by "<companyId>/<userId>". Phone tabs always sit
/// right after Platform. Runs on the main thread.
fn open_tab_ex(app: &AppHandle, url: Url, background: bool, pin_owner: Option<String>) {
    // Quitting: no new tabs (a delayed reopen from reconnect_voice or
    // ensure_phone_tab can land here mid-exit).
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    let window = match app.get_window("main") {
        Some(w) => w,
        None => return,
    };

    // Dedupe the singleton tabs: re-opening Phone or Phone settings (sidebar
    // re-click, click-to-call fallback) FOCUSES the existing tab instead of
    // stacking a duplicate — two live CCPs would be two competing softphones.
    let is_settings_url = url.path().rsplit('/').find(|s| !s.is_empty()) == Some("voice-settings");
    if is_voice_url(&url) || is_settings_url {
        let candidates = {
            let s = app.state::<Mutex<TabState>>();
            let s = s.lock().unwrap();
            if is_settings_url {
                s.settings.clone()
            } else {
                s.voice.clone()
            }
        };
        for l in candidates {
            if app.get_webview(&l).is_some() {
                if let (Some(owner), false) = (pin_owner.clone(), is_settings_url) {
                    pin_tab(app, &l, owner);
                }
                if !background {
                    activate(app, &l);
                    let _ = app.emit("tab:focused", serde_json::json!({ "label": l }));
                }
                return;
            }
        }
    }

    // Dedupe Messaging / POS too (owner 10-28: every sidebar click stacked
    // another Messaging tab through the day). Matched on the tab's CURRENT
    // url, so a tab that navigated within its section still counts. A bare
    // re-open just focuses the tab. A Messaging link with a target
    // (?clientId=…) also navigates the existing tab to it — Messaging holds
    // no unsaved work. A POS link with a target (?saleId=…) still gets its
    // own tab so it never replaces a sale someone is ringing up.
    if let Some(section) = singleton_section(&url) {
        let targeted = url.query().is_some_and(|q| !q.is_empty());
        if !(section == "pos" && targeted) {
            let candidates = {
                let s = app.state::<Mutex<TabState>>();
                let s = s.lock().unwrap();
                s.order
                    .iter()
                    .filter(|l| *l != PLATFORM_TAB)
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for l in candidates {
                let Some(wv) = app.get_webview(&l) else {
                    continue;
                };
                let same = wv
                    .url()
                    .ok()
                    .is_some_and(|cur| singleton_section(&cur) == Some(section));
                if same {
                    if targeted {
                        let _ = wv.navigate(url.clone());
                    }
                    activate(app, &l);
                    let _ = app.emit("tab:focused", serde_json::json!({ "label": l }));
                    return;
                }
            }
        }
    }

    let label = format!("tab-{}", TAB_COUNTER.fetch_add(1, Ordering::Relaxed));
    let (lw, lh) = logical_inner(&window).unwrap_or((INITIAL_WIDTH, INITIAL_HEIGHT));
    let content_h = (lh - TITLE_BAR_HEIGHT).max(0.0);

    let nw_app = app.clone();
    let nw_label = label.clone();
    let nav_app = app.clone();
    let nav_label = label.clone();
    let builder = WebviewBuilder::new(label.as_str(), WebviewUrl::External(url.clone()))
        // A tab opened in the background (the always-on Phone tab, its
        // rebuilds) must not take the keyboard: whatever the person types in
        // the tab they are on stays there (v0.3.3).
        .focused(!background)
        // Tabs can themselves open further tabs / auth popups / external links.
        .on_new_window(move |u, _features| decide_new_window(&nw_app, &nw_label, u))
        // Mark the desktop app, resolve the native dialog globals the dialog
        // plugin clobbers (from the capture plugin's stash — see
        // DIALOG_CAPTURE_SCRIPT), bridge window.close() → sentinel nav, and
        // give the softphone page its call-active hook (and the Platform-only
        // restart hook). desktop_marker_script() stays FIRST so the dialog
        // diagnostic can read the app version off window.__VISTA_DESKTOP__.
        // 5 placeholders, 5 args.
        .initialization_script(format!(
            "{}{}{}{}{}",
            desktop_marker_script(),
            DIALOG_GLOBALS_SCRIPT,
            CLOSE_BRIDGE_SCRIPT,
            VOICE_BRIDGE_SCRIPT,
            restart_bridge_script()
        ))
        .on_navigation(move |nav_url| {
            if nav_url.as_str().starts_with(CLOSE_SENTINEL) {
                // Destroying this web view from inside its own navigation
                // callback is exactly the reentrancy `later` avoids.
                let l = nav_label.clone();
                later(&nav_app, move |app| close_tab_impl(app, &l, false));
                return false; // cancel the sentinel nav; the tab is closing
            }
            if let Some(flag) = nav_url.as_str().strip_prefix(CALL_SENTINEL) {
                set_call_active(&nav_app, flag.starts_with('1'));
                return false; // cancel the sentinel nav; the page stays put
            }
            if nav_url.as_str().starts_with(AGENT_READY_SENTINEL) {
                later(&nav_app, close_login_window);
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

    // Phone-tab self-recovery (v0.3.3): watch this Phone tab's page loads from
    // now on, before its first load can finish.
    if is_voice_url(&url) {
        phone_recovery::attach(app, &label, pin_owner.is_some());
    }

    let is_voice = is_voice_url(&url);
    let pinned = is_voice && pin_owner.is_some();
    let (index, current_active) = {
        let state = app.state::<Mutex<TabState>>();
        let mut s = state.lock().unwrap();
        // Phone sits right after Platform (owner 2026-11-18); others append.
        let index = if is_voice {
            1.min(s.order.len())
        } else {
            s.order.len()
        };
        s.order.insert(index, label.clone());
        if is_voice {
            s.voice.push(label.clone());
        } else if is_settings_url {
            s.settings.push(label.clone());
        }
        if pinned {
            s.pinned.push(label.clone());
            s.phone_owner = pin_owner.clone();
        }
        (index, s.active.clone())
    };
    if background {
        // Re-lay out around the tab the person is on: the new Phone tab is
        // parked off-screen (voice tabs are never hidden) behind theirs.
        activate(app, &current_active);
    } else {
        // Show the new tab, hide the rest, mark it active.
        activate(app, &label);
    }

    let _ = app.emit(
        "tab:opened",
        serde_json::json!({
            "label": label,
            "title": tab_title(&url),
            "index": index,
            "closeable": !pinned,
            "pinned": pinned,
            "phone": is_voice,
            "background": background,
        }),
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
    // Switching to (or away from) the Phone tab hides (or shows) the pop-up.
    refresh_popup(app);
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
    let (is_voice, is_pinned) = {
        let state = app.state::<Mutex<TabState>>();
        let s = state.lock().unwrap();
        (
            s.voice.iter().any(|v| v == label),
            s.pinned.iter().any(|v| v == label),
        )
    };
    // The pinned Phone tab has no ×, and a page's window.close() can't close
    // it either: only the shell does (sign-out, a hard reconnect, another
    // person's phone), with `force` (owner 2026-11-18).
    if is_pinned && !force {
        return;
    }
    if is_voice && !force && CALL_ACTIVE.load(Ordering::SeqCst) {
        let _ = app.emit(
            "call:close-blocked",
            serde_json::json!({ "kind": "tab", "label": label }),
        );
        return;
    }
    if is_voice {
        // This web view's page events are stale from here on (v0.3.3).
        phone_recovery::on_webview_closed(label);
    }
    let reactivate = {
        let state = app.state::<Mutex<TabState>>();
        let mut s = state.lock().unwrap();
        if let Some(idx) = s.order.iter().position(|l| l == label) {
            s.order.remove(idx);
        }
        s.voice.retain(|v| v != label);
        s.settings.retain(|v| v != label);
        s.pinned.retain(|v| v != label);
        // The page that armed the call flag is going away — clear it so a
        // stale flag can't block closes forever.
        if is_voice {
            set_call_active(app, false);
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
        if is_voice {
            // The Phone tab's Amazon Connect CCP keeps a shared worker + login
            // session alive in the app's SHARED WebView2 profile — state that
            // outlives this webview. Destroying the webview abruptly (as the
            // else-branch does) never runs the page's connect.core.terminate(),
            // so the orphaned state jams the NEXT Phone tab on "Initializing…".
            // Platform-side terminate-before-init can't clear it (a fresh page's
            // terminate doesn't reach the orphan) — it must be torn down while
            // its OWN webview is still alive. So: terminate CCP in-page, give it
            // a beat to reach the shared worker, THEN destroy the webview.
            let _ = wv.eval(TERMINATE_PHONE_JS);
            let app_close = app.clone();
            let label_close = label.to_string();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(PHONE_TEARDOWN_MS));
                let app_inner = app_close.clone();
                let _ = app_close.run_on_main_thread(move || {
                    if let Some(wv) = app_inner.get_webview(&label_close) {
                        let _ = wv.close();
                    }
                });
            });
        } else {
            let _ = wv.close();
        }
    }
    if is_voice {
        // The page that announced a ring is gone, and so is its pop-up.
        *RINGING.lock().unwrap() = None;
        hide_popup(app);
    }
    if let Some(next) = &reactivate {
        activate(app, next);
    }
    let active_now = app
        .state::<Mutex<TabState>>()
        .lock()
        .unwrap()
        .active
        .clone();
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
    let active = app
        .state::<Mutex<TabState>>()
        .lock()
        .unwrap()
        .active
        .clone();
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
    let _ = app
        .clone()
        .run_on_main_thread(move || activate(&app, &label));
}

#[tauri::command]
fn close_tab(app: AppHandle, label: String, force: Option<bool>) {
    let force = force.unwrap_or(false);
    let _ = app
        .clone()
        .run_on_main_thread(move || close_tab_impl(&app, &label, force));
}

// Invoked from the incoming-call pop-up (popup.html, local origin).

/// The call the pop-up should show (asked once popup.html has loaded).
#[tauri::command]
fn popup_current() -> Option<serde_json::Value> {
    RINGING.lock().unwrap().clone()
}

/// popup.html has drawn the call: show the window now, unless the ring ended
/// or the Phone tab came to the front meanwhile.
#[tauri::command]
fn popup_ready(app: AppHandle) {
    let _ = app.clone().run_on_main_thread(move || refresh_popup(&app));
}

/// Pop-up "Answer": bring VISTA to the front on the Phone tab and pick up.
#[tauri::command]
fn popup_answer(app: AppHandle, contact_id: String) {
    let _ = app.clone().run_on_main_thread(move || {
        hide_popup(&app);
        if EXITING.load(Ordering::SeqCst) {
            return; // the softphone is already being ended
        }
        let Some(label) = phone_tab_label(&app) else {
            return;
        };
        if let Some(main) = app.get_window("main") {
            let _ = main.unminimize();
            let _ = main.show();
            let _ = main.set_focus();
        }
        activate(&app, &label);
        let _ = app.emit("tab:focused", serde_json::json!({ "label": label }));
        call_phone_page(&app, &label, "__VISTA_ANSWER_CALL", &contact_id);
    });
}

/// Pop-up "Ignore": stop ringing here for this one call and pass it on, the
/// same as Decline (owner 2026-11-18). The next call still pops up.
#[tauri::command]
fn popup_ignore(app: AppHandle, contact_id: String) {
    let _ = app.clone().run_on_main_thread(move || {
        {
            let mut ringing = RINGING.lock().unwrap();
            let same = ringing
                .as_ref()
                .and_then(|r| r.get("contactId"))
                .and_then(|v| v.as_str())
                .is_some_and(|cur| cur == contact_id);
            if same {
                *ringing = None;
            }
        }
        hide_popup(&app);
        if let Some(label) = phone_tab_label(&app) {
            call_phone_page(&app, &label, "__VISTA_IGNORE_CALL", &contact_id);
        }
    });
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

/// The title bar's ✕: a normal close request, so the main window's
/// CloseRequested handler decides (live-call guard, otherwise quit).
#[tauri::command]
fn window_close(window: tauri::Window) -> tauri::Result<()> {
    window.close()
}

/// "Close anyway" in the title bar's confirm strip: the person chose to hang
/// up, so skip the live-call guard — but still end the Phone tab's softphone
/// first and then really quit (v0.3.1; it used to destroy() the window, which
/// dropped the Phone tab without terminate and left the process running).
#[tauri::command]
fn window_force_close(app: AppHandle) {
    later(&app, |app| begin_exit(app, false));
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
