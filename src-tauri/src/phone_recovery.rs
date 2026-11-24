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
// with backoff (10 s, 30 s, 60 s, 2 min, then every 5 min). A VISTA page that
// answers is never rebuilt, whatever its path; one that stops answering is
// rebuilt after 2 minutes only with no call flagged and only if its softphone
// never came up. The rules are in phone_recovery/logic.rs (unit-tested in CI).
//
// Restart (owner decision 2026-11-23, "Only when safe"): only the FIRST
// softphone of an app run comes up; every later one in the same run stays
// stuck. So after a dead tab VISTA restarts itself through the v0.3.1 restart
// path (begin_exit with relaunch), but only with no call and no ring, nobody
// at the computer for 10 minutes (an hour while any VISTA web view is on the
// POS or an appointment checkout, the screens that can start a card-reader
// charge: v0.3.4), Windows not shutting down, and, when the
// failure points at the server or the network, only after a page has answered
// again; at most once per 30 minutes and 3 times in 6 hours (kept on disk and
// written BEFORE the restart; a failed write means no restart). Until then the
// tab is rebuilt and the restart stays pending. A softphone that reports ready
// cancels it, and so do sign-out and quitting. The relaunched copy reopens the
// Phone tab from a one-shot hand-off file, even behind the server's idle lock.
//
// The same safety test gates an update's install that had to wait for a call
// (v0.3.4, wait_for_install_moment): Install pressed during a call, or a ring
// during the download, no longer installs the moment the call ends (that
// could close a POS or checkout still waiting on the card reader, and drop
// what was typed during the call) but at the next moment the restart itself
// would be allowed, with the same hand-off (per-user NSIS installs only; an
// MSI install, which asks for admin rights, drops it until the next launch).
// An install with no call when its download finishes still starts at once,
// as the person asked.
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
//   the app (or a call) down. Once the exit has begun, a caught panic
//   finishes the exit instead (a hidden window with EXITING set would ignore
//   every close).

mod logic;

use std::fs;
use std::io::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock, TryLockError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager, Url};

// One definition of the Platform host, a VISTA id and the sentinel prefix,
// shared with lib.rs.
pub(crate) use self::logic::{is_vista_id, PLATFORM_HOST, SENTINEL_PREFIX};
use self::logic::{
    handoff_json, history_from_read, history_json, install_held_line, is_payment_path,
    normalize_history, parse_handoff, parse_history, restart_decision, safety_hold, split_owner,
    unambiguous, utc_stamp, voice_url_for_owner, Action, ContentEvt, HistoryRead, HoldLog, NavDone,
    NavStart, ProbeOutcome, RestartHold, Snapshot, Trigger, Watch, INSTALL_ANSWER_TIMEOUT,
    INSTALL_CHECK_EVERY, INSTALL_POLL, PERIOD, REBUILD_GAP_MS, RESTART_CAP, RESTART_COOLDOWN_MS,
    TICK,
};

use super::{
    begin_exit, call_or_ringing, close_login_window, close_phone_tabs, close_tab_impl,
    ensure_phone_tab, finish_exit, is_voice_url, later, later_after, open_tab_ex, phone_tab_label,
    refresh_popup, set_call_active, TabState, CALL_ACTIVE, EXITING, EXIT_SENT, PLATFORM_TAB,
    RINGING,
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
/// The app, for the one thing a caught panic must still do once the exit has
/// begun: leave.
static APP: OnceLock<Mutex<AppHandle>> = OnceLock::new();

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

/// Run `f`; a panic is logged (as a fixed word, never its text) and
/// `fallback` returned.
fn guarded<R>(what: &str, fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            log(format!("panic where={what} kind={}", panic_kind(&*payload)));
            leave_if_exiting(what);
            fallback
        }
    }
}

/// The exit began (EXITING is set) but its last step has not run: run it now,
/// so a panic can never leave a hidden window that ignores every close.
fn leave_if_exiting(what: &str) {
    if !EXITING.load(Ordering::SeqCst) || EXIT_SENT.load(Ordering::SeqCst) {
        return;
    }
    let app = APP
        .get()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).clone());
    if let Some(app) = app {
        log_sync(format!("panic during exit where={what}: leaving now"));
        finish_exit(&app);
    }
}

/// What kind of panic, as a fixed word: never any text of the payload (it can
/// carry names, numbers or paths).
fn panic_kind(payload: &(dyn std::any::Any + Send)) -> &'static str {
    let text: &str = if let Some(s) = payload.downcast_ref::<&str>() {
        *s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        ""
    };
    if text.contains("char boundary") || text.contains("byte index") {
        "slice"
    } else if text.contains("poison") {
        "poison"
    } else if text.contains("out of bounds") {
        "index"
    } else if text.contains("unwrap") {
        "unwrap"
    } else if text.contains("borrow") {
        "borrow"
    } else if text.contains("overflow") {
        "overflow"
    } else if text.is_empty() {
        "unknown"
    } else {
        "other"
    }
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
    let _ = APP.set(Mutex::new(app.clone()));
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
            Action::RestartHeld { word, suppressed } => {
                log(held_line(&word, "", suppressed, idle_ms_now()))
            }
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
        let created_now =
            match try_with_watch("attach", |w| w.on_webview_created(now, label, gen, pinned)) {
                Some(actions) => {
                    log_only(actions);
                    true
                }
                None => {
                    later(app, move |_app| {
                        let actions =
                            with_watch(|w| w.on_webview_created(now, &owned, gen, pinned));
                        log_only(actions);
                    });
                    false
                }
            };
        #[cfg(windows)]
        let hooks_ok = attach_com(app, label, gen);
        #[cfg(not(windows))]
        let hooks_ok = false;
        // Whether silence can mean "never loaded" here: reported after the
        // creation, in the same order (`later` is FIFO).
        let reported = if created_now {
            try_with_watch("hooks", |w| w.on_hooks(gen, hooks_ok))
        } else {
            None
        };
        match reported {
            Some(actions) => log_only(actions),
            None => later(app, move |_app| {
                let actions = with_watch(|w| w.on_hooks(gen, hooks_ok));
                log_only(actions);
            }),
        }
    });
}

/// Our own NavigationStarting, ContentLoading, NavigationCompleted and
/// ProcessFailed handlers on the Phone web view (wry registers its own and
/// throws the details away). They fire on the UI thread, only read their
/// arguments and queue work; they live as long as the web view (as the mic
/// handler does). If one fails to register, the periodic probe still covers
/// the tab. Returns whether the three load-event handlers registered.
#[cfg(windows)]
fn attach_com(app: &AppHandle, label: &str, gen: u64) -> bool {
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
        return false;
    };
    let app_start = app.clone();
    let app_content = app.clone();
    let app_done = app.clone();
    let app_fail = app.clone();
    let (tx, rx) = mpsc::channel::<bool>();
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
                    let _ = tx.send(false);
                    return;
                }
            };
            let mut token = 0i64;
            // The three load events silence is judged by.
            let mut loads_hooked = true;

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
                loads_hooked = false;
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
                loads_hooked = false;
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
                loads_hooked = false;
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
            let _ = tx.send(loads_hooked);
        }
    });
    if let Err(e) = result {
        log(format!("attach gen={gen} with_webview-failed {e}"));
        return false;
    }
    // with_webview ran us inline on the main thread (lib.rs watch_engine_exit
    // relies on the same). Never block waiting: a queued closure would need
    // this very thread.
    rx.try_recv().unwrap_or(false)
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

/// The page's own reconnect sentinel (reconnect_voice, `from` = the tab that
/// sent it), on the main thread: only from the Phone tab itself (the bridge
/// script runs in every tab and frame, and this force-closes the Phone tab),
/// one rebuild in flight at a time, none for a person who signed out.
pub(crate) fn page_asked_reconnect(app: &AppHandle, from: &str) {
    guarded("page-reconnect", (), || {
        if EXITING.load(Ordering::SeqCst) {
            return;
        }
        match phone_tab_label(app) {
            None => {
                log("page reconnect: no Phone tab");
                return;
            }
            Some(phone) if phone != from => {
                log("page reconnect ignored: not the Phone tab");
                return;
            }
            Some(_) => {}
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

/// This copy is restarting itself on its own (lib.rs close_all_then_exit then
/// waits longer for the old engine to exit).
pub(crate) fn restarting() -> bool {
    RESTARTING.load(Ordering::SeqCst)
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
    let label_live = label
        .as_deref()
        .is_some_and(|l| app.get_webview(l).is_some());
    let call = CALL_ACTIVE.load(Ordering::SeqCst);
    let page_asked = trigger == Trigger::PageAsked;
    let why = if call && platform_on_phone_page(app) {
        // A call live in the Platform tab (on the Phone page itself, lib.rs
        // 1111-1114) keeps its flag, and closing a Phone tab would clear it.
        Some("platform-tab-on-phone-page")
    } else if call && !page_asked && !unambiguous(trigger) {
        // A death a live call could survive is never rebuilt while a call is
        // flagged (the page's own reconnect says no call is live in it).
        Some("call")
    } else if !page_asked && label_live && !pinned && trigger != Trigger::Missing {
        // The watchdog only ever rebuilds the always-on tab.
        Some("not-pinned")
    } else {
        None
    };
    if let Some(why) = why {
        let actions = with_watch(|w| w.cancel_rebuild(id, why));
        run_actions(app, actions);
        return;
    }
    let wanted = with_watch(|w| w.wanted());
    let pin_owner = if pinned || label.is_none() || !page_asked {
        wanted.or(owner)
    } else {
        None
    };
    let url: Option<Url> = match &pin_owner {
        Some(owner) => voice_url_for_owner(owner).and_then(|u| u.parse::<Url>().ok()),
        // A page-asked rebuild of an unpinned Phone tab: as before (its
        // address), but only while that is still the Phone page (a load that
        // never arrived reports about:blank: rule 2a).
        None => label
            .as_deref()
            .and_then(|l| app.get_webview(l))
            .and_then(|wv| wv.url().ok())
            .filter(is_voice_url),
    };
    let Some(url) = url else {
        let actions = with_watch(|w| w.cancel_rebuild(id, "no-url"));
        run_actions(app, actions);
        return;
    };
    let gen = with_watch(|w| w.cur_gen()).map_or_else(|| "-".to_string(), |g| g.to_string());
    let background = !was_active;
    // "Missing", but a Phone tab was opened from the sidebar during the wait:
    // it is never closed (it may carry a call); the open step adopts it.
    let adopt = trigger == Trigger::Missing && label_live;
    log(format!(
        "rebuild start id={id} gen={gen} trigger={} tab={} active={} pinned={} adopt={}",
        trigger.word(),
        label.as_deref().unwrap_or("none"),
        was_active as u8,
        pin_owner.is_some() as u8,
        adopt as u8
    ));
    if adopt {
        open_step(app, id, url, background, pin_owner);
        return;
    }
    // A stalled sign-in window belongs to the app, not the tab (as the page's
    // own reconnect does).
    close_login_window(app);
    if unambiguous(trigger) {
        clear_stale_call(app);
    }
    if let Some(l) = &label {
        // Ends the softphone in-page, drops the web view 700 ms later, clears
        // the call flag and the ring (close_tab_impl).
        close_tab_impl(app, l, true);
    }
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
        let has_owner = {
            let state = app.state::<Mutex<TabState>>();
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.phone_owner.is_some()
        };
        let (allowed, owner_gone) = with_watch(|w| (w.open_step_allowed(id), w.owner_gone()));
        // A sign-out ends the ticket itself (on_signed_out, go_idle). This
        // second check also asks the watchdog, so a first open that failed
        // (phone_owner never set) is still retried.
        let signed_out = pin_owner.is_some() && owner_gone && !has_owner;
        let why = if !allowed {
            Some("superseded")
        } else if signed_out {
            Some("signed-out")
        } else {
            None
        };
        if let Some(why) = why {
            let actions = with_watch(|w| w.cancel_rebuild(id, why));
            run_actions(app, actions);
            return;
        }
        // A Phone tab opened from the sidebar meanwhile (unpinned): open_tab_ex
        // adopts it (dedupe, then pinned), but only one on this owner's spa;
        // it is never closed (it may carry a call).
        let before = phone_tab_label(app).filter(|l| app.get_webview(l).is_some());
        if let (Some(owner), Some(existing)) = (pin_owner.as_deref(), before.as_deref()) {
            if !tab_on_owner_spa(app, existing, owner) {
                let actions = with_watch(|w| w.finish_rebuild(id, "tab-appeared"));
                run_actions(app, actions);
                return;
            }
        }
        open_tab_ex(app, url, background, pin_owner);
        // attach → on_webview_created ended the ticket if the always-on tab
        // was created. Otherwise it adopted a tab, opened an unpinned one, or
        // opened none (add_child failed; the "missing" path takes over): the
        // ticket ends now.
        if with_watch(|w| w.open_step_allowed(id)) {
            let after = phone_tab_label(app);
            let how = match (&before, &after) {
                (_, None) => "not-opened",
                (Some(b), Some(a)) if b == a => "adopted",
                _ => "opened-unwatched",
            };
            let actions = with_watch(|w| w.finish_rebuild(id, how));
            run_actions(app, actions);
        }
    });
}

/// The tab `label` shows a page of `owner`'s spa (/manage/<company>/...).
fn tab_on_owner_spa(app: &AppHandle, label: &str, owner: &str) -> bool {
    let Some((company, _)) = split_owner(owner) else {
        return false;
    };
    let prefix = format!("/manage/{}/", company.to_ascii_lowercase());
    app.get_webview(label)
        .and_then(|wv| wv.url().ok())
        .is_some_and(|u| u.path().to_ascii_lowercase().starts_with(&prefix))
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

/// Any web view of the app (the Platform tab, any other tab, any window) shows
/// a Platform screen that can start a card-reader charge (v0.3.4: the POS, an
/// appointment checkout; logic::is_payment_path). Main thread, like every
/// wv.url() read in lib.rs. The address is only matched, never logged.
fn payment_page_open(app: &AppHandle) -> bool {
    app.webviews()
        .into_values()
        .filter_map(|wv| wv.url().ok())
        .any(|u| u.host_str() == Some(PLATFORM_HOST) && is_payment_path(u.path()))
}

// ─── The restart ("Only when safe") ──────────────────────────────────

/// The automatic restart's safety test, right now (logic::safety_hold): no
/// call and no ring (a poisoned ring lock counts as ringing: lib.rs
/// call_or_ringing); nobody at the computer for 10 minutes, or for an hour
/// while any VISTA web view is on a screen that can start a card-reader charge
/// (v0.3.4: closing it mid-charge would leave a charged card on an unpaid
/// sale; the web views are looked at only when it can matter, and the hold
/// word is "payment-page"); Windows not shutting down. None: safe. The restart
/// (try_restart) and an update install that had to wait (install_check) both
/// use it. Main thread.
fn safety_hold_now(app: &AppHandle, idle: Option<u32>) -> Option<&'static str> {
    safety_hold(call_or_ringing(), idle, || payment_page_open(app), shutting_down)
}

/// Whose Phone tab a restart (or an update's install) hands to the next copy:
/// the person the watchdog keeps it for, else the pinned tab's owner.
fn phone_owner(app: &AppHandle) -> Option<String> {
    with_watch(|w| w.wanted()).or_else(|| {
        let state = app.state::<Mutex<TabState>>();
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.phone_owner.clone()
    })
}

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
    // Read first: every decision line carries it.
    let idle = idle_ms_now();
    // No call and no ring, nobody at the computer for 10 minutes (an hour
    // while a payment screen is open), Windows not shutting down: the test an
    // update's install that had to wait uses too (safety_hold_now).
    if let Some(word) = safety_hold_now(app, idle) {
        restart_held(now, word, "", idle);
        return;
    }
    // The cooldown and the cap, kept on disk: written and read back before
    // the restart, or no restart at all.
    let now_ms = unix_ms();
    let count = match record_restart(now_ms) {
        Ok(count) => count,
        Err((word, extra)) => {
            restart_held(now, word, &extra, idle);
            return;
        }
    };
    if let Some(owner) = phone_owner(app) {
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
    // the engine exits, then a fresh copy starts (the sign-in is kept). It
    // sets EXITING first, so if it panics part-way, leave anyway (and
    // relaunch): a hidden window with EXITING set ignores every close.
    if catch_unwind(AssertUnwindSafe(|| begin_exit(app, true))).is_err() {
        log_sync("restart: begin_exit failed part-way, leaving now");
        finish_exit(app);
    }
}

/// A restart held (a call, someone at the computer, a payment screen, the
/// cooldown, ...): logged at most once per reason per 10 minutes, with the
/// idle time and the cooldown state.
fn restart_held(now: Instant, word: &str, extra: &str, idle: Option<u32>) {
    if let Some(suppressed) = with_watch(|w| w.restart_hold_due(now, word)) {
        log(held_line(word, extra, suppressed, idle));
    }
}

/// "restart held reason=<word><extra> idle=<s> restarts-6h=<n> since-last=<s>
/// [repeats-suppressed=<n>]": every restart decision says why, how long
/// nobody has touched the computer, and where the cooldown and the cap stand.
fn held_line(word: &str, extra: &str, suppressed: u32, idle: Option<u32>) -> String {
    let rep = if suppressed > 0 {
        format!(" repeats-suppressed={suppressed}")
    } else {
        String::new()
    };
    format!("restart held reason={word}{extra}{}{rep}", restart_state(idle))
}

/// " idle=<s|unknown> restarts-6h=<n> since-last=<s|->", from the restart
/// record read the way the cooldown and the cap read it (never written here).
fn restart_state(idle: Option<u32>) -> String {
    let idle = idle.map_or_else(|| "unknown".to_string(), |ms| format!("{}s", ms / 1000));
    let now_ms = unix_ms();
    let (raw, _) = history_from_read(&read_history(), now_ms);
    let (list, _) = normalize_history(&raw, now_ms);
    let since = list.iter().max().map_or_else(
        || "-".to_string(),
        |&t| format!("{}s", now_ms.saturating_sub(t) / 1000),
    );
    format!(" idle={idle} restarts-6h={} since-last={since}", list.len())
}

/// The restart record as it is on disk.
fn read_history() -> HistoryRead {
    let Some(path) = data_file(RESTARTS_FILE) else {
        return HistoryRead::Missing;
    };
    match fs::read_to_string(path) {
        Ok(text) => HistoryRead::Text(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HistoryRead::Missing,
        Err(_) => HistoryRead::Unreadable,
    }
}

/// Check the cooldown and the cap against the record, then write the record
/// with this restart and read it back. Err = do not restart.
fn record_restart(now_ms: u64) -> Result<usize, (&'static str, String)> {
    let Some(path) = data_file(RESTARTS_FILE) else {
        return Err(("record", " step=no-folder".to_string()));
    };
    let (raw, broken) = history_from_read(&read_history(), now_ms);
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
            // (The line also gets restarts-6h and since-last: restart_state.)
            Err(match hold {
                RestartHold::Cooldown { since_last_s } => (
                    "cooldown",
                    format!(
                        " cooldown-left={}s",
                        (RESTART_COOLDOWN_MS / 1000).saturating_sub(since_last_s)
                    ),
                ),
                RestartHold::Cap { count } => ("cap", format!(" cap={count}/{RESTART_CAP}")),
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

// ─── An update's install (v0.3.4) ────────────────────────────────────

/// What the safety test said, on the main thread, for a waiting install.
enum InstallCheck {
    /// Install now, handing the Phone tab to this person (if any).
    Go(Option<String>, Option<u32>),
    /// Keep waiting: the hold word, and the idle time for its line.
    Hold(&'static str, Option<u32>),
}

/// The automatic restart's safety test for an update install that had to
/// wait (safety_hold_now), with whose Phone tab to hand over when it passes.
/// Main thread: it reads the web views' addresses. Posted with `later` by
/// wait_for_install_moment, never run inside a WebView2 callback.
fn install_check(app: &AppHandle) -> InstallCheck {
    guarded("install-check", InstallCheck::Hold("check-failed", None), || {
        if EXITING.load(Ordering::SeqCst) {
            return InstallCheck::Hold("quit", None);
        }
        let idle = idle_ms_now();
        match safety_hold_now(app, idle) {
            Some(word) => InstallCheck::Hold(word, idle),
            None => InstallCheck::Go(phone_owner(app), idle),
        }
    })
}

/// Which installer this copy of the app came from (tauri-cli patches the
/// bundle type into each installer's copy of the exe).
fn installer_word() -> &'static str {
    use tauri::utils::{config::BundleType, platform::bundle_type};
    match bundle_type() {
        Some(BundleType::Nsis) => "nsis",
        Some(BundleType::Msi) => "msi",
        Some(_) => "other",
        None => "unknown",
    }
}

/// Block the calling plain thread (never the main thread, never a worker of
/// the async runtime) until a downloaded update may install (lib.rs
/// spawn_update_check). true: install now (the line saying so is already in
/// the log). false: dropped (logged too), and the box comes back at the next
/// launch: the app began quitting first, or the install would have had to run
/// unattended from an installer that asks for admin rights.
///
/// No call and no ring when the download finished: at once, as the person
/// just asked ("Install and restart now?"). Otherwise the install would come
/// at a moment nobody chose (pre-release check M1): closing VISTA the second
/// the call ends could close a POS or an appointment checkout still waiting on
/// the card reader (the tap charges the card, capture automatic, but only that
/// page records the sale) and would drop what was typed during the call. So
/// it waits for the automatic restart's own safety test: no call and no ring,
/// nobody at the computer for 10 minutes (an hour while a payment screen is
/// open), Windows not shutting down. The test reads web view addresses, so it
/// runs on the main thread (install_check); this thread only waits for the
/// answer, looks at the call flags once more, and hands the Phone tab to the
/// copy the installer starts (as the restart does), so it reopens even behind
/// the server's idle lock.
///
/// Waiting means installing with nobody at the computer, which only the
/// per-user NSIS install (bundle installMode currentUser) can do: it never
/// asks Windows for admin rights. An MSI install is per-machine and would sit
/// on a UAC prompt nobody answers, with VISTA already closed, so any other
/// installer drops the update instead of waiting.
pub(crate) fn wait_for_install_moment(app: &AppHandle) -> bool {
    let started = Instant::now();
    if EXITING.load(Ordering::SeqCst) {
        log("update install dropped reason=quit");
        return false;
    }
    if !call_or_ringing() {
        log_sync("update install now waited=0s");
        return true;
    }
    let installer = installer_word();
    if installer != "nsis" {
        log(format!("update install dropped reason=call installer={installer}"));
        return false;
    }
    log("update install waiting reason=call");
    let mut last_check: Option<Instant> = None;
    let mut holds = HoldLog::new();
    loop {
        std::thread::sleep(INSTALL_POLL);
        if EXITING.load(Ordering::SeqCst) {
            log("update install dropped reason=quit");
            return false;
        }
        // During a call or a ring the main thread is never asked.
        if call_or_ringing() || last_check.is_some_and(|t| t.elapsed() < INSTALL_CHECK_EVERY) {
            continue;
        }
        last_check = Some(Instant::now());
        let (tx, rx) = mpsc::channel::<InstallCheck>();
        later(app, move |app| {
            let _ = tx.send(install_check(app));
        });
        match rx.recv_timeout(INSTALL_ANSWER_TIMEOUT) {
            Ok(InstallCheck::Go(owner, idle)) => {
                // Safe in that main-thread turn. The flags once more, right
                // before the installer takes over (a ring may have just come).
                if EXITING.load(Ordering::SeqCst) {
                    log("update install dropped reason=quit");
                    return false;
                }
                if call_or_ringing() {
                    continue;
                }
                let handoff = match owner {
                    Some(owner) => match write_handoff(&owner, unix_ms()) {
                        Ok(()) => true,
                        Err(e) => {
                            log_sync(format!("handoff write failed kind={:?}", e.kind()));
                            false
                        }
                    },
                    None => false,
                };
                log_sync(format!(
                    "update install now waited={}s idle={}s handoff={}",
                    started.elapsed().as_secs(),
                    idle.unwrap_or(0) / 1000,
                    handoff as u8
                ));
                return true;
            }
            Ok(InstallCheck::Hold(word, idle)) => {
                if let Some(suppressed) = holds.due(Instant::now(), word) {
                    log(install_held_line(word, suppressed, idle));
                }
            }
            // No answer in time (a busy main thread): ask again at the next
            // check.
            Err(_) => {}
        }
    }
}

/// update.install failed (lib.rs): VISTA keeps running, so a hand-off written
/// for it is withdrawn.
pub(crate) fn install_failed() {
    log("update install failed");
    if let Some(path) = data_file(HANDOFF_FILE) {
        let _ = fs::remove_file(path);
    }
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

/// The same, written before returning: the lines that explain a restart (or
/// an update's install, lib.rs) must reach the file before the process ends.
pub(crate) fn log_sync(msg: impl Into<String>) {
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
