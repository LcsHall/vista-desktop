// Phone-tab self-recovery (v0.3.3): the shell's safety net for a Phone tab
// whose page never arrived or died.
//
// Why: all of the Phone tab's self-repair is JavaScript inside the voice page,
// so a load that fails (the load balancer's 502 page, a load cut off inside
// the app, Chromium's error page, about:blank) has no voice page to repair it,
// and the tab stayed dead until someone restarted the app (2026-11-20..23: one
// 502 page for 25 h, four cut-off loads for 2 to 43 h).
//
// What: every Phone web view gets its own NavigationStarting, ContentLoading,
// NavigationCompleted and ProcessFailed handlers (wry's own throw the details
// away), and a watchdog asks the tab every minute, with a tiny read-only
// script, whether a VISTA page is actually running in it. A dead tab (two "no
// page here" answers 10 s apart, a committed error page that stays silent, a
// load that never arrived, a dead renderer, or no Phone tab at all) is rebuilt
// for its owner from /manage/<company>/voice, pinned and in the same place,
// with backoff (10 s, 30 s, 60 s, 2 min, then every 5 min). A live VISTA page
// of any path is never rebuilt. The rules are in phone_recovery/logic.rs
// (unit-tested in CI).
//
// Restart (owner decision 2026-11-23, "Only when safe"): only the FIRST
// softphone of an app run comes up; every later one in the same run stays
// stuck. So after a dead tab VISTA restarts itself through the v0.3.1 restart
// path (begin_exit with relaunch), but only with no call and no ring, nobody
// at the computer for 10 minutes, the server answering, Windows not shutting
// down, at most once per 30 minutes and 3 times in 6 hours (kept on disk and
// written BEFORE the restart; a failed write means no restart). Until then the
// tab is rebuilt and the restart stays pending. A softphone that reports ready
// cancels it, and so do sign-out and quitting. The relaunched copy reopens the
// Phone tab from a one-shot hand-off file, even behind the server's idle lock.
//
// Nothing visible is added (the restart's own close-and-reopen aside). Log:
// %LOCALAPPDATA%\com.vistainterface.platform\phone-tab.log (512 KiB plus one
// older file): path templates, kind words, status codes, error names,
// durations and counters only. Never an address, an id, a name or a number.
//
// Wiring rules:
// - Never call into Tauri or WebView2 while holding WATCH: it is not
//   re-entrant, and close_tab_impl calls back into on_webview_closed. Always
//   `let r = with_watch(..);` first, then act on `r`.
// - Hooks that run inside other shell code (attach, on_webview_closed,
//   on_ensure, on_signed_out, page_alive, page_state) only change state and
//   log, and only try the lock. Probes, rebuilds, call-flag clears and
//   restarts happen only from the tick and the handlers posted with `later`,
//   so no web view is ever created or destroyed inside a WebView2 callback.
// - Every entry point runs under catch_unwind: a panic here must never take
//   the app (or a call) down.

mod logic;

use std::fs;
use std::io::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock, TryLockError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager, Url};

pub(crate) use self::logic::SENTINEL_PREFIX;
use self::logic::{
    handoff_json, history_from_read, history_json, normalize_history, parse_handoff,
    parse_history, restart_decision, utc_stamp, voice_url_for_owner, Action, ContentEvt,
    HistoryRead, NavDone, NavStart, ProbeOutcome, RestartHold, Snapshot, Trigger, Watch, PERIOD,
    REBUILD_GAP_MS, RESTART_IDLE_MS, TICK,
};

use super::{
    begin_exit, close_login_window, close_phone_tabs, close_tab_impl, ensure_phone_tab,
    is_voice_url, later, later_after, open_tab_ex, phone_tab_label, refresh_popup,
    set_call_active, TabState, CALL_ACTIVE, EXITING, PLATFORM_TAB, RINGING,
};

static WATCH: Mutex<Watch> = Mutex::new(Watch::new());
/// One generation per Phone web view: events and probe answers from a replaced
/// web view never count for its successor.
static NEXT_GEN: AtomicU64 = AtomicU64::new(1);
/// A tick is already queued on the main thread.
static TICK_QUEUED: AtomicBool = AtomicBool::new(false);
/// %LOCALAPPDATA%\com.vistainterface.platform
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static LOG_TX: OnceLock<Mutex<mpsc::Sender<String>>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
/// Serialises the writer thread and the few synchronous writes.
static LOG_FILE: Mutex<()> = Mutex::new(());
/// This copy is restarting itself (for the exit-path note).
static RESTARTING: AtomicBool = AtomicBool::new(false);
static EXIT_NOTED: AtomicBool = AtomicBool::new(false);

const LOG_FILE_NAME: &str = "phone-tab.log";
const LOG_MAX_BYTES: u64 = 512 * 1024;
/// The automatic restarts of the last 6 hours (Unix ms, JSON array).
const RESTARTS_FILE: &str = "phone-restarts.json";
/// The one-shot hand-off a restart leaves for the next copy.
const HANDOFF_FILE: &str = "phone-handoff.json";

// ─── Locking and panic safety ────────────────────────────────────────

/// Run `f` on the watchdog state. Bind the result first, then act on it:
/// never hold WATCH across a call into Tauri.
fn with_watch<R>(f: impl FnOnce(&mut Watch) -> R) -> R {
    let mut guard = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut *guard)
}

/// `with_watch` for the hooks that run inside other shell code: never blocks.
/// (With the rules above the lock is never held there; this only makes sure a
/// mistake can never freeze the UI thread.)
fn try_with_watch<R>(hook: &str, f: impl FnOnce(&mut Watch) -> R) -> Option<R> {
    let mut guard = match WATCH.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => {
            log(format!("reentrant hook={hook}"));
            return None;
        }
    };
    Some(f(&mut *guard))
}

/// Run `f`; a panic is logged (sanitised) and `fallback` returned.
fn guarded<R>(what: &str, fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            log(format!("panic where={what} msg={}", panic_text(&*payload)));
            fallback
        }
    }
}

/// A panic message reduced to letters and a little punctuation (no digits,
/// so no number can ever reach the log).
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    let raw = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    raw.chars()
        .filter(|&c| c.is_ascii_alphabetic() || matches!(c, ' ' | ':' | '_' | '.' | '-' | '(' | ')'))
        .take(80)
        .collect()
}

/// Only the log lines of `actions` (what the hooks may do).
fn log_only(actions: Vec<Action>) {
    for action in actions {
        if let Action::Log(msg) = action {
            log(msg);
        }
    }
}

// ─── Start and the tick ──────────────────────────────────────────────

/// Start the watchdog and its log. Called once from the app's setup, after
/// TabState is managed.
pub(crate) fn start(app: &AppHandle) {
    guarded("start", (), || {
        if let Ok(dir) = app.path().app_local_data_dir() {
            let _ = fs::create_dir_all(&dir);
            start_log_writer(dir.join(LOG_FILE_NAME));
            let _ = DATA_DIR.set(dir);
        }
        log(format!(
            "start version={} tick={}s period={}s restarts-6h={}",
            env!("CARGO_PKG_VERSION"),
            TICK.as_secs(),
            PERIOD.as_secs(),
            restarts_in_window()
        ));
        // The copy before this one restarted itself and handed its Phone tab
        // over: reopen it now, even if the server's idle lock keeps the
        // Platform tab from asking for it (the lock still allows the voice
        // page). A later sign-out or another person's sign-in behaves as ever.
        if let Some((company, user)) = take_handoff() {
            log("handoff: reopening the Phone tab after an automatic restart");
            later(app, move |app| {
                if !EXITING.load(Ordering::SeqCst) {
                    ensure_phone_tab(app, company, user);
                }
            });
        }
        let tick_app = app.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(TICK);
            if EXITING.load(Ordering::SeqCst) {
                log("stop reason=quit");
                break;
            }
            // At most one tick waiting: a busy main thread never finds a pile.
            if !TICK_QUEUED.swap(true, Ordering::SeqCst) {
                later(&tick_app, tick);
            }
        });
    });
}

/// One watchdog tick, on the main thread.
fn tick(app: &AppHandle) {
    TICK_QUEUED.store(false, Ordering::SeqCst);
    guarded("tick", (), || {
        if EXITING.load(Ordering::SeqCst) {
            return;
        }
        let snapshot = snapshot(app);
        let now = Instant::now();
        let actions = with_watch(|w| w.on_tick(now, &snapshot));
        run_actions(app, actions);
    });
}

/// What the tick needs from TabState and the flags (TabState is released
/// before WATCH is taken).
fn snapshot(app: &AppHandle) -> Snapshot {
    let (owner, phone, phone_pinned) = {
        let state = app.state::<Mutex<TabState>>();
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        let phone = s.voice.first().cloned();
        let pinned = phone
            .as_deref()
            .is_some_and(|l| s.pinned.iter().any(|p| p == l));
        (s.phone_owner.clone(), phone, pinned)
    };
    let phone_has_webview = phone
        .as_deref()
        .is_some_and(|l| app.get_webview(l).is_some());
    Snapshot {
        exiting: EXITING.load(Ordering::SeqCst),
        owner,
        phone,
        phone_pinned,
        phone_has_webview,
        call_active: CALL_ACTIVE.load(Ordering::SeqCst),
    }
}

/// Carry out what the rules asked for, in order, outside the WATCH lock.
fn run_actions(app: &AppHandle, actions: Vec<Action>) {
    for action in actions {
        if EXITING.load(Ordering::SeqCst) {
            if let Action::Log(msg) = action {
                log(msg);
            }
            continue;
        }
        match action {
            Action::Log(msg) => log(msg),
            Action::Probe { label, gen, id } => send_probe(app, &label, gen, id),
            Action::RunRebuild { id, trigger } => run_rebuild(app, id, trigger),
            Action::ClearStaleCall => clear_stale_call(app),
            Action::FinishSignOut => close_phone_tabs(app),
            Action::TryRestart => try_restart(app),
        }
    }
}

// ─── The Phone web view's own events (WebView2 COM) ──────────────────

/// A Phone web view was just created: open_tab_ex, right after add_child, in
/// the same main-thread turn, so no event of its first load can be missed.
pub(crate) fn attach(app: &AppHandle, label: &str, pinned: bool) {
    guarded("attach", (), || {
        let gen = NEXT_GEN.fetch_add(1, Ordering::SeqCst);
        let now = Instant::now();
        let owned = label.to_string();
        match try_with_watch("attach", |w| w.on_webview_created(now, label, gen, pinned)) {
            Some(actions) => log_only(actions),
            None => later(app, move |_app| {
                let actions = with_watch(|w| w.on_webview_created(now, &owned, gen, pinned));
                log_only(actions);
            }),
        }
        #[cfg(windows)]
        attach_com(app, label, gen);
    });
}

/// Our own NavigationStarting, ContentLoading, NavigationCompleted and
/// ProcessFailed handlers on the Phone web view (wry registers its own and
/// throws the details away). They fire on the UI thread, only read their
/// arguments and queue work; they live as long as the web view (as the mic
/// handler does). If one fails to register, the periodic probe still covers
/// the tab.
#[cfg(windows)]
fn attach_com(app: &AppHandle, label: &str, gen: u64) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2NavigationCompletedEventArgs2, COREWEBVIEW2_PROCESS_FAILED_KIND,
        COREWEBVIEW2_WEB_ERROR_STATUS,
    };
    use webview2_com::{
        take_pwstr, ContentLoadingEventHandler, NavigationCompletedEventHandler,
        NavigationStartingEventHandler, ProcessFailedEventHandler,
    };
    use windows::core::{Interface, BOOL, PWSTR};

    let Some(webview) = app.get_webview(label) else {
        log(format!("attach gen={gen} tab={label} no-webview"));
        return;
    };
    let app_start = app.clone();
    let app_content = app.clone();
    let app_done = app.clone();
    let app_fail = app.clone();
    let result = webview.with_webview(move |platform_webview| {
        // SAFETY: WebView2 COM calls on the UI thread with_webview runs us on
        // (inline: open_tab_ex runs on the main thread). The handlers fire on
        // that thread too; they only read their arguments and queue work.
        unsafe {
            log_engine_version_once(&platform_webview.environment());
            let core = match platform_webview.controller().CoreWebView2() {
                Ok(core) => core,
                Err(e) => {
                    log(format!(
                        "attach gen={gen} core-failed hr={:#010x}",
                        e.code().0 as u32
                    ));
                    return;
                }
            };
            let mut token = 0i64;

            let on_start =
                NavigationStartingEventHandler::create(Box::new(move |_sender, args| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        let Some(args) = args else { return };
                        let at = Instant::now();
                        let mut uri = PWSTR::null();
                        let uri = if args.Uri(&mut uri).is_ok() {
                            take_pwstr(uri)
                        } else {
                            String::new()
                        };
                        let mut id = 0u64;
                        let _ = args.NavigationId(&mut id);
                        let mut redirected = BOOL::default();
                        let _ = args.IsRedirected(&mut redirected);
                        let start = NavStart {
                            id,
                            at,
                            sentinel: logic::is_sentinel(&uri),
                            // The path template only: the address stops here.
                            target: logic::log_target(&uri),
                            redirected: redirected.as_bool(),
                        };
                        later(&app_start, move |app| on_nav_starting(app, gen, start));
                    }));
                    Ok(())
                }));
            if let Err(e) = core.add_NavigationStarting(&on_start, &mut token) {
                log(format!(
                    "attach gen={gen} nav-start-hook-failed hr={:#010x}",
                    e.code().0 as u32
                ));
            }

            // A document committed; IsErrorPage = Chromium's own error page
            // replaced the old document (an ExecuteScript may never answer
            // there, so this is the evidence that it is gone).
            let on_content = ContentLoadingEventHandler::create(Box::new(move |_sender, args| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let Some(args) = args else { return };
                    let at = Instant::now();
                    let mut error_page = BOOL::default();
                    let _ = args.IsErrorPage(&mut error_page);
                    let mut id = 0u64;
                    let _ = args.NavigationId(&mut id);
                    let evt = ContentEvt {
                        id,
                        at,
                        error_page: error_page.as_bool(),
                    };
                    later(&app_content, move |app| on_content_loading(app, gen, evt));
                }));
                Ok(())
            }));
            if let Err(e) = core.add_ContentLoading(&on_content, &mut token) {
                log(format!(
                    "attach gen={gen} content-hook-failed hr={:#010x}",
                    e.code().0 as u32
                ));
            }

            // A second NavigationCompleted handler: wry's own throws the
            // arguments away, so this is the only way to read the status.
            let on_done =
                NavigationCompletedEventHandler::create(Box::new(move |sender, args| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        let Some(args) = args else { return };
                        let at = Instant::now();
                        let mut ok = BOOL::default();
                        let _ = args.IsSuccess(&mut ok);
                        let mut err = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                        let _ = args.WebErrorStatus(&mut err);
                        let mut id = 0u64;
                        let _ = args.NavigationId(&mut id);
                        // HttpStatusCode is on Args2; an older runtime fails
                        // the cast and simply reports no status.
                        let http = args
                            .cast::<ICoreWebView2NavigationCompletedEventArgs2>()
                            .ok()
                            .and_then(|a2| {
                                let mut code = 0i32;
                                a2.HttpStatusCode(&mut code).ok().map(|()| code)
                            });
                        // The kind of page now shown, never the address.
                        let source = sender
                            .as_ref()
                            .and_then(|wv| {
                                let mut src = PWSTR::null();
                                wv.Source(&mut src).ok().map(|()| take_pwstr(src))
                            })
                            .map(|s| logic::url_kind(&s));
                        let done = NavDone {
                            id,
                            at,
                            ok: ok.as_bool(),
                            err: err.0,
                            http,
                            source,
                        };
                        later(&app_done, move |app| on_nav_completed(app, gen, done));
                    }));
                    Ok(())
                }));
            if let Err(e) = core.add_NavigationCompleted(&on_done, &mut token) {
                log(format!(
                    "attach gen={gen} nav-done-hook-failed hr={:#010x}",
                    e.code().0 as u32
                ));
            }

            let on_fail = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let Some(args) = args else { return };
                    let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                    let _ = args.ProcessFailedKind(&mut kind);
                    let kind = kind.0;
                    later(&app_fail, move |app| on_process_failed(app, gen, kind));
                }));
                Ok(())
            }));
            if let Err(e) = core.add_ProcessFailed(&on_fail, &mut token) {
                log(format!(
                    "attach gen={gen} process-hook-failed hr={:#010x}",
                    e.code().0 as u32
                ));
            }
        }
    });
    if let Err(e) = result {
        log(format!("attach gen={gen} with_webview-failed {e}"));
    }
}

/// The WebView2 runtime version, once per launch (whether HttpStatusCode and
/// responseStatus can exist on this desk). Call on the UI thread.
#[cfg(windows)]
unsafe fn log_engine_version_once(
    env: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment,
) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut version = windows::core::PWSTR::null();
    // SAFETY: a COM getter on the UI thread; the string is freed by take_pwstr.
    if unsafe { env.BrowserVersionString(&mut version) }.is_ok() {
        let version: String = webview2_com::take_pwstr(version)
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == ' ')
            .take(40)
            .collect();
        log(format!("engine webview2={version}"));
    }
}

/// Run the probe in the Phone tab's main frame. Raw ExecuteScript (not
/// eval_with_callback) so a failure's HRESULT reaches the log. No answer at
/// all is the timeout's business.
#[cfg(windows)]
fn send_probe(app: &AppHandle, label: &str, gen: u64, id: u64) {
    use webview2_com::ExecuteScriptCompletedHandler;
    use windows::core::HSTRING;

    let Some(webview) = app.get_webview(label) else {
        later(app, move |app| {
            on_probe_answer(app, gen, id, ProbeOutcome::NoWebview)
        });
        return;
    };
    let app_cb = app.clone();
    let result = webview.with_webview(move |platform_webview| {
        // SAFETY: as in attach_com. ExecuteScript returns at once; its
        // completion handler runs later on the UI thread and only queues work.
        unsafe {
            let core = match platform_webview.controller().CoreWebView2() {
                Ok(core) => core,
                Err(e) => {
                    let hr = e.code().0;
                    later(&app_cb, move |app| {
                        on_probe_answer(app, gen, id, ProbeOutcome::Failed(hr))
                    });
                    return;
                }
            };
            let app_done = app_cb.clone();
            let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, json| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let outcome = match result {
                        Ok(()) => ProbeOutcome::Json(json),
                        Err(e) => ProbeOutcome::Failed(e.code().0),
                    };
                    later(&app_done, move |app| on_probe_answer(app, gen, id, outcome));
                }));
                Ok(())
            }));
            let js = HSTRING::from(logic::PROBE_JS);
            if let Err(e) = core.ExecuteScript(&js, &handler) {
                let hr = e.code().0;
                later(&app_cb, move |app| {
                    on_probe_answer(app, gen, id, ProbeOutcome::Failed(hr))
                });
            }
        }
    });
    if let Err(e) = result {
        log(format!("probe gen={gen} id={id} with_webview-failed {e}"));
    }
}

#[cfg(not(windows))]
fn send_probe(_app: &AppHandle, _label: &str, _gen: u64, _id: u64) {}

fn on_nav_starting(app: &AppHandle, gen: u64, start: NavStart) {
    guarded("nav-start", (), || {
        let actions = with_watch(|w| w.on_nav_starting(gen, start));
        run_actions(app, actions);
    });
}

fn on_content_loading(app: &AppHandle, gen: u64, evt: ContentEvt) {
    guarded("content", (), || {
        let now = Instant::now();
        let actions = with_watch(|w| w.on_content_loading(now, gen, evt));
        run_actions(app, actions);
    });
}

fn on_nav_completed(app: &AppHandle, gen: u64, done: NavDone) {
    guarded("nav-done", (), || {
        let now = Instant::now();
        let actions = with_watch(|w| w.on_nav_completed(now, gen, done));
        run_actions(app, actions);
    });
}

fn on_process_failed(app: &AppHandle, gen: u64, kind: i32) {
    guarded("process-failed", (), || {
        let now = Instant::now();
        let call_active = CALL_ACTIVE.load(Ordering::SeqCst);
        let actions = with_watch(|w| w.on_process_failed(now, gen, kind, call_active));
        run_actions(app, actions);
    });
}

fn on_probe_answer(app: &AppHandle, gen: u64, id: u64, outcome: ProbeOutcome) {
    guarded("probe", (), || {
        let now = Instant::now();
        let call_active = CALL_ACTIVE.load(Ordering::SeqCst);
        let actions = with_watch(|w| w.on_probe_answer(now, gen, id, outcome, call_active));
        run_actions(app, actions);
    });
}

// ─── Hooks from the rest of the shell ────────────────────────────────

/// close_tab_impl: this web view's events are stale from here on.
pub(crate) fn on_webview_closed(label: &str) {
    guarded("closed", (), || {
        if let Some(actions) = try_with_watch("closed", |w| w.on_webview_closed(label)) {
            log_only(actions);
        }
    });
}

/// ensure_phone_tab: false while our rebuild of this person's tab is already
/// on its way (ensure must not open a second one).
pub(crate) fn on_ensure(app: &AppHandle, owner: &str) -> bool {
    guarded("ensure", true, || {
        let current = {
            let state = app.state::<Mutex<TabState>>();
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.phone_owner.clone()
        };
        let now = Instant::now();
        match try_with_watch("ensure", |w| w.on_ensure(now, owner, current.as_deref())) {
            Some((proceed, actions)) => {
                log_only(actions);
                proceed
            }
            None => true,
        }
    })
}

/// close_phone_tabs (sign-out): stop rebuilding and cancel a pending restart,
/// even when a live call keeps the tab.
pub(crate) fn on_signed_out(_app: &AppHandle) {
    guarded("signed-out", (), || {
        let now = Instant::now();
        if let Some(actions) = try_with_watch("signed-out", |w| w.on_signed_out(now)) {
            log_only(actions);
        }
        if let Some(path) = data_file(HANDOFF_FILE) {
            let _ = fs::remove_file(path);
        }
    });
}

/// Any bridge sentinel from `from` (decide_new_window): its page is running.
pub(crate) fn page_alive(from: &str) {
    guarded("page-alive", (), || {
        let now = Instant::now();
        if let Some(actions) = try_with_watch("page-alive", |w| w.on_page_alive(now, from)) {
            log_only(actions);
        }
    });
}

/// PHONE_STATE from `from` (available | call | offline | connecting): a ready
/// softphone cancels a pending restart.
pub(crate) fn page_state(from: &str, state: &str) {
    guarded("page-state", (), || {
        let now = Instant::now();
        if let Some(actions) = try_with_watch("page-state", |w| w.on_phone_state(now, from, state))
        {
            log_only(actions);
        }
    });
}

/// The page's own reconnect sentinel (reconnect_voice), on the main thread:
/// one rebuild in flight at a time, none for a person who signed out.
pub(crate) fn page_asked_reconnect(app: &AppHandle) {
    guarded("page-reconnect", (), || {
        if EXITING.load(Ordering::SeqCst) {
            return;
        }
        if phone_tab_label(app).is_none() {
            log("page reconnect: no Phone tab");
            return;
        }
        let now = Instant::now();
        let (ticket, actions) = with_watch(|w| w.on_page_reconnect(now));
        run_actions(app, actions);
        if let Some(id) = ticket {
            run_rebuild(app, id, Trigger::PageAsked);
        }
    });
}

/// The restart path's last steps (lib.rs): which way this copy left, when it
/// was restarting itself (whether the relaunch can get a fresh engine).
pub(crate) fn note_exit(how: &str) {
    if RESTARTING.load(Ordering::SeqCst) && !EXIT_NOTED.swap(true, Ordering::SeqCst) {
        log_sync(format!("restart exit path={how}"));
    }
}

// ─── The rebuild ─────────────────────────────────────────────────────

/// Close the Phone tab and reopen it like the page's own reconnect does, but
/// for its owner from /manage/<company>/voice (a load that never arrived most
/// likely reports about:blank as its address), pinned, in the same place.
/// Runs on the main thread, outside the WATCH lock.
fn run_rebuild(app: &AppHandle, id: u64, trigger: Trigger) {
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    let (label, was_active, pinned, owner) = {
        let state = app.state::<Mutex<TabState>>();
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        let label = s.voice.first().cloned();
        let was_active = label.as_deref().is_some_and(|l| l == s.active);
        let pinned = label
            .as_deref()
            .is_some_and(|l| s.pinned.iter().any(|p| p == l));
        (label, was_active, pinned, s.phone_owner.clone())
    };
    // A call live in the Platform tab (on the Phone page itself, lib.rs
    // 1111-1114) keeps its flag, and closing a Phone tab would clear it.
    if CALL_ACTIVE.load(Ordering::SeqCst) && platform_on_phone_page(app) {
        let actions = with_watch(|w| w.cancel_rebuild(id, "platform-tab-on-phone-page"));
        run_actions(app, actions);
        return;
    }
    let wanted = with_watch(|w| w.wanted());
    let pin_owner = if pinned || label.is_none() {
        wanted.or(owner)
    } else {
        None
    };
    let url: Option<Url> = match &pin_owner {
        Some(owner) => voice_url_for_owner(owner).and_then(|u| u.parse::<Url>().ok()),
        // A page-asked rebuild of an unpinned Phone tab: as before (its URL).
        None => label
            .as_deref()
            .and_then(|l| app.get_webview(l))
            .and_then(|wv| wv.url().ok()),
    };
    let Some(url) = url else {
        let actions = with_watch(|w| w.cancel_rebuild(id, "no-url"));
        run_actions(app, actions);
        return;
    };
    // A stalled sign-in window belongs to the app, not the tab (as the page's
    // own reconnect does).
    close_login_window(app);
    if trigger.definite() {
        clear_stale_call(app);
    }
    if let Some(l) = &label {
        // Ends the softphone in-page, drops the web view 700 ms later, clears
        // the call flag and the ring (close_tab_impl).
        close_tab_impl(app, l, true);
    }
    log(format!(
        "rebuild start id={id} trigger={} tab={} active={} pinned={}",
        trigger.word(),
        label.as_deref().unwrap_or("none"),
        was_active as u8,
        pin_owner.is_some() as u8
    ));
    let background = !was_active;
    let gap = if label.is_some() { REBUILD_GAP_MS } else { 0 };
    later_after(app, gap, move |app| {
        open_step(app, id, url, background, pin_owner)
    });
}

/// The second half of a rebuild, once the old web view is gone.
fn open_step(app: &AppHandle, id: u64, url: Url, background: bool, pin_owner: Option<String>) {
    guarded("open-step", (), || {
        if EXITING.load(Ordering::SeqCst) {
            return;
        }
        let signed_in = pin_owner.is_none() || {
            let state = app.state::<Mutex<TabState>>();
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.phone_owner.is_some()
        };
        let allowed = with_watch(|w| w.open_step_allowed(id));
        let why = if !allowed {
            Some("superseded")
        } else if !signed_in {
            Some("signed-out")
        } else {
            None
        };
        if let Some(why) = why {
            let actions = with_watch(|w| w.cancel_rebuild(id, why));
            run_actions(app, actions);
            return;
        }
        // attach → on_webview_created marks the rebuild opened. If open_tab_ex
        // deduped onto an existing tab or add_child failed, the rebuild
        // expires after 30 s and the "missing" path takes over.
        open_tab_ex(app, url, background, pin_owner);
    });
}

/// No live page is left in the Phone tab, so no call can be live in it: clear
/// the call flag and a stale ring the way close_tab_impl does (rule 2c).
fn clear_stale_call(app: &AppHandle) {
    // CALL_ACTIVE is ONE global flag: if the Platform tab itself is on the
    // Phone page, a call may live there. Leave it.
    if platform_on_phone_page(app) {
        log("call-flag kept reason=platform-tab-on-phone-page");
        return;
    }
    let had_call = CALL_ACTIVE.load(Ordering::SeqCst);
    let had_ring = RINGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .is_some();
    if had_call {
        set_call_active(app, false); // also releases the sleep block
    }
    if had_ring {
        refresh_popup(app); // RINGING is empty now: hides the pop-up
    }
    if had_call || had_ring {
        log(format!(
            "call-flag cleared call={} ring={}",
            had_call as u8, had_ring as u8
        ));
    }
}

fn platform_on_phone_page(app: &AppHandle) -> bool {
    app.get_webview(PLATFORM_TAB)
        .and_then(|wv| wv.url().ok())
        .is_some_and(|u| is_voice_url(&u))
}

// ─── The restart ("Only when safe") ──────────────────────────────────

/// Restart now if it is safe; otherwise log why not (throttled) and let the
/// tick ask again. Every check and begin_exit run in this one main-thread
/// turn, so nothing can change in between.
fn try_restart(app: &AppHandle) {
    let now = Instant::now();
    let Some(reason) = with_watch(|w| w.restart_reason()) else {
        return; // canceled meanwhile
    };
    if EXITING.load(Ordering::SeqCst) {
        return; // the person is already quitting: never turn that into a relaunch
    }
    let ringing = RINGING.lock().map(|r| r.is_some()).unwrap_or(true);
    if CALL_ACTIVE.load(Ordering::SeqCst) || ringing {
        restart_held(now, "call", String::new());
        return;
    }
    let idle = idle_ms_now();
    match idle {
        Some(ms) if ms >= RESTART_IDLE_MS => {}
        Some(ms) => {
            restart_held(now, "active", format!(" idle={}s", ms / 1000));
            return;
        }
        None => {
            restart_held(now, "idle-unknown", String::new());
            return;
        }
    }
    if shutting_down() {
        restart_held(now, "shutdown", String::new());
        return;
    }
    // The cooldown and the cap, kept on disk: written and read back before
    // the restart, or no restart at all.
    let now_ms = unix_ms();
    let count = match record_restart(now_ms) {
        Ok(count) => count,
        Err((word, detail)) => {
            restart_held(now, word, detail);
            return;
        }
    };
    let owner = with_watch(|w| w.wanted()).or_else(|| {
        let state = app.state::<Mutex<TabState>>();
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.phone_owner.clone()
    });
    if let Some(owner) = owner {
        if let Err(e) = write_handoff(&owner, now_ms) {
            log_sync(format!("handoff write failed kind={:?}", e.kind()));
        }
    }
    log_sync(format!(
        "restart now reason={reason} idle={}s restarts-6h={count} cooldown=ok",
        idle.unwrap_or(0) / 1000
    ));
    RESTARTING.store(true, Ordering::SeqCst);
    // The v0.3.1 restart: the softphone ends in-page, every web view closes,
    // the engine exits, then a fresh copy starts (the sign-in is kept).
    begin_exit(app, true);
}

fn restart_held(now: Instant, word: &str, detail: String) {
    let line = with_watch(|w| w.restart_held(now, word, &detail));
    if let Some(line) = line {
        log(line);
    }
}

/// Check the cooldown and the cap against the record, then write the record
/// with this restart and read it back. Err = do not restart.
fn record_restart(now_ms: u64) -> Result<usize, (&'static str, String)> {
    let Some(path) = data_file(RESTARTS_FILE) else {
        return Err(("record", " step=no-folder".to_string()));
    };
    let read = match fs::read_to_string(&path) {
        Ok(text) => HistoryRead::Text(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HistoryRead::Missing,
        Err(_) => HistoryRead::Unreadable,
    };
    let (raw, broken) = history_from_read(&read, now_ms);
    let (list, changed) = normalize_history(&raw, now_ms);
    match restart_decision(&list, now_ms) {
        Ok(next) => {
            if write_atomic(&path, &history_json(&next)).is_err() {
                return Err(("record", " step=write".to_string()));
            }
            let back = fs::read_to_string(&path)
                .ok()
                .and_then(|text| parse_history(&text));
            if !back.is_some_and(|list| list.contains(&now_ms)) {
                return Err(("record", " step=verify".to_string()));
            }
            Ok(next.len())
        }
        Err(hold) => {
            if broken || changed {
                // Heal it (an unreadable file counts as "restarted just now").
                let _ = write_atomic(&path, &history_json(&list));
            }
            if broken {
                return Err(("record-reset", String::new()));
            }
            Err(match hold {
                RestartHold::Cooldown { since_last_s } => {
                    ("cooldown", format!(" since-last={since_last_s}s"))
                }
                RestartHold::Cap { count } => ("cap", format!(" restarts-6h={count}")),
            })
        }
    }
}

/// Write `text` to a temporary file next to `path`, then move it into place
/// (fs::rename replaces the old file on Windows).
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

fn write_handoff(owner: &str, now_ms: u64) -> std::io::Result<()> {
    let path = data_file(HANDOFF_FILE)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no data folder"))?;
    write_atomic(&path, &handoff_json(owner, now_ms))
}

/// The previous copy's hand-off (read once, then deleted).
fn take_handoff() -> Option<(String, String)> {
    let path = data_file(HANDOFF_FILE)?;
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    parse_handoff(&text, unix_ms())
}

/// Automatic restarts in the last 6 hours (for the start line).
fn restarts_in_window() -> usize {
    let Some(path) = data_file(RESTARTS_FILE) else {
        return 0;
    };
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    parse_history(&text).map_or(0, |list| normalize_history(&list, unix_ms()).0.len())
}

fn data_file(name: &str) -> Option<PathBuf> {
    DATA_DIR.get().map(|dir| dir.join(name))
}

/// Milliseconds since the last keyboard or mouse input in this Windows
/// session; None (counts as NOT idle) when Windows can't say.
#[cfg(windows)]
fn idle_ms_now() -> Option<u32> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: plain Win32 calls; `info` is a valid LASTINPUTINFO with cbSize set.
    let ok = unsafe { GetLastInputInfo(&mut info) }.as_bool();
    if !ok {
        return None;
    }
    // Both are 32-bit tick counts (they wrap every 49.7 days): wrapping math.
    let now = unsafe { GetTickCount() };
    Some(logic::idle_ms(now, info.dwTime))
}

#[cfg(not(windows))]
fn idle_ms_now() -> Option<u32> {
    None
}

/// Windows is shutting down or signing out: never start a new copy then.
#[cfg(windows)]
fn shutting_down() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
    // SAFETY: a plain Win32 call with no pointers.
    unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
}

#[cfg(not(windows))]
fn shutting_down() -> bool {
    false
}

// ─── The log (phone-tab.log; nothing personal, ever) ─────────────────

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn stamp_line(msg: String) -> String {
    format!("{} {msg}", utc_stamp(u128::from(unix_ms())))
}

/// One line in phone-tab.log, written by the log thread: no file I/O on the
/// UI thread or inside a WebView2 callback. (And on stderr in a dev build.)
pub(crate) fn log(msg: impl Into<String>) {
    let line = stamp_line(msg.into());
    #[cfg(debug_assertions)]
    eprintln!("[phone-tab] {line}");
    if let Some(tx) = LOG_TX.get() {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(line);
        }
    }
}

/// The same, written before returning: the lines that explain a restart must
/// reach the file before the process ends.
fn log_sync(msg: impl Into<String>) {
    let line = stamp_line(msg.into());
    #[cfg(debug_assertions)]
    eprintln!("[phone-tab] {line}");
    if let Some(path) = LOG_PATH.get() {
        append_line(path, &line);
    }
}

fn start_log_writer(path: PathBuf) {
    let (tx, rx) = mpsc::channel::<String>();
    if LOG_TX.set(Mutex::new(tx)).is_err() {
        return;
    }
    let _ = LOG_PATH.set(path.clone());
    std::thread::spawn(move || {
        for line in rx {
            append_line(&path, &line);
        }
    });
}

/// Append one line; past LOG_MAX_BYTES the file becomes phone-tab.log.1 and a
/// new one starts (about 1 MiB in all).
fn append_line(path: &Path, line: &str) {
    let _guard = LOG_FILE.lock().unwrap_or_else(|e| e.into_inner());
    if fs::metadata(path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}
