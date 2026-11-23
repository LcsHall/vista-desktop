//! Phone-tab self-recovery: the pure decision rules (v0.3.3).
//!
//! Plain data in, decisions out. No Tauri, no WebView2, no files and no clock
//! of its own (every function is handed `now`), so all of it runs under
//! `cargo test`: the tests are at the bottom, and CI runs them through
//! `src-tauri/tests/phone_recovery_logic.rs`, which includes this file by path
//! (that test binary links nothing of Tauri). The wiring that feeds these rules
//! from the app is `../phone_recovery.rs`.
//!
//! Panic safety (this runs on the UI thread, possibly mid-call):
//! - never `Instant - Duration` (it panics in the first minutes after a
//!   reboot): ages are `now.saturating_duration_since(t)`, deadlines `t + d`;
//! - no indexing into strings or slices; tables are `match`es with a
//!   catch-all arm.
//!
//! Nothing personal goes into a log line built here: path TEMPLATES (ids become
//! `:id`, digit runs `:n`), kind words, status codes, error names, durations
//! and counters only.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

// ─── Numbers (every timing in one place) ─────────────────────────────

/// How often the watchdog looks at the Phone tab.
pub const TICK: Duration = Duration::from_secs(5);
/// Probe cadence for a healthy Phone page.
pub const PERIOD: Duration = Duration::from_secs(60);
/// Never judge a page within this long of a navigation start (or creation).
pub const NAV_GRACE: Duration = Duration::from_secs(20);
/// The second "dead" answer must come at least this long after the first.
pub const CONFIRM_GAP: Duration = Duration::from_secs(10);
/// A VISTA document still not complete this long after it began loading
/// (ContentLoading), with no app running and no progress, is dead.
pub const LOAD_LIMIT: Duration = Duration::from_secs(45);
/// A navigation in flight this long is no longer just "loading" (the load
/// balancer may legitimately take up to its 60 s idle timeout).
pub const IN_FLIGHT_LIMIT: Duration = Duration::from_secs(75);
/// A probe with no answer after this long counts as "no answer". A late
/// answer still counts as a sign of life.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Re-probe cadence while probes go unanswered.
pub const NO_ANSWER_RETRY: Duration = Duration::from_secs(15);
/// Re-probe cadence while a page is still loading.
pub const LOADING_RETRY: Duration = Duration::from_secs(10);
/// A page that answers nothing is rebuilt only after this long, and never
/// while a call is flagged.
pub const UNRESPONSIVE_LIMIT: Duration = Duration::from_secs(120);
/// An owner with no Phone tab must be seen this long before the tab is
/// re-created (ensure_phone_tab's own replace gap is 1.5 s).
pub const MISSING_CONFIRM: Duration = Duration::from_secs(10);
/// Close-to-open gap of a rebuild (the page's own reconnect used 900 ms).
pub const REBUILD_GAP_MS: u64 = 2000;
/// A rebuild whose new tab never appeared is abandoned after this long.
pub const OPEN_LOST: Duration = Duration::from_secs(30);
/// Probe cadence of a dead tab while its rebuild waits.
pub const WAIT_PROBE: Duration = Duration::from_secs(15);
/// After the network comes back, the next rebuild waits at most this long...
pub const NET_BACK_DELAY: Duration = Duration::from_secs(3);
/// ...if the flip was seen within this long.
pub const NET_BACK_WINDOW: Duration = Duration::from_secs(120);
/// The backoff starts over only after the page stayed alive this long.
pub const STABLE_AFTER: Duration = Duration::from_secs(300);
/// After this many rebuilds in one dead streak, say the engine may be stuck.
pub const STUCK_AFTER: u32 = 3;
/// A navigation storm: this many starts...
pub const STORM_STARTS: usize = 10;
/// ...within this long stop renewing the grace.
pub const STORM_WINDOW: Duration = Duration::from_secs(300);
/// One "alive" probe line at most this often.
pub const HEARTBEAT_LOG: Duration = Duration::from_secs(1800);
/// The same not-alive probe line at most this often.
pub const REPEAT_LOG: Duration = Duration::from_secs(60);
/// Navigations remembered per web view.
pub const NAV_MEMORY: usize = 16;

// The automatic restart (owner decision 2026-11-23, "Only when safe").

/// Nobody has touched the computer for this long (Windows idle time).
pub const RESTART_IDLE_MS: u32 = 600_000;
/// At most one automatic restart per this long...
pub const RESTART_COOLDOWN_MS: u64 = 30 * 60 * 1000;
/// ...and at most RESTART_CAP of them within this long (kept on disk, so a
/// relaunch remembers them).
pub const RESTART_WINDOW_MS: u64 = 6 * 60 * 60 * 1000;
pub const RESTART_CAP: usize = 3;
/// A pending restart is re-checked this often.
pub const RESTART_CHECK: Duration = Duration::from_secs(30);
/// After a renderer exit a restart waits at least this long (a Windows
/// shutdown can end the engine before it ends the app).
pub const RESTART_MIN_PENDING_RENDERER: Duration = Duration::from_secs(60);
/// The same restart-hold line at most this often.
pub const HOLD_LOG_EVERY: Duration = Duration::from_secs(600);
/// The restart hand-off is honoured only this soon after it was written.
pub const HANDOFF_MAX_AGE_MS: u64 = 120_000;

/// The shell's own sentinel URLs (lib.rs: the close, call and phone bridges).
pub const SENTINEL_PREFIX: &str = "https://vista-desktop.invalid/";
/// The Platform host (the same as lib.rs PLATFORM_HOST).
pub const PLATFORM_HOST: &str = "platform.vistainterface.com";

/// The liveness probe, run in the Phone tab's main frame with ExecuteScript.
/// Read-only: it never writes to the page and is not user activity. It
/// answers (as JSON):
/// - `k` page kind: `p` the Platform host, `v` another vistainterface.com host,
///   `b` about:blank, `e` any non-https page (Chromium's error page), `o` any
///   other https host, `x` the probe itself threw;
/// - `n` the Next.js runtime started (`window.next.version`, set by Next's
///   app bootstrap); `c` the voice page's own softphone hook exists
///   (`__VISTA_ANSWER_CALL`: the page defines it, never the shell); `q` Amazon's
///   Streams library is loaded (`connect.core`); `a` its agent arrived
///   (`connect.agent.initialized`: true once the softphone got its agent,
///   false again after a terminate, which a halted page always runs);
/// - `r` document.readyState; `s` this document's HTTP status; `t` how many
///   resources it has loaded so far (progress); `o` navigator.onLine;
/// - `p` the path, VISTA hosts only (templated at once, never stored).
///
/// A `/_next/static/` script tag is deliberately NOT a sign of life: a chunk
/// that failed during a deploy leaves exactly that on a dead page.
pub const PROBE_JS: &str = r#"(function(){try{var w=window,l=w.location,p=String(l.protocol),h=String(l.hostname).toLowerCase(),k='o';if(p==='about:'){k='b'}else if(p!=='https:'){k='e'}else if(h==='platform.vistainterface.com'){k='p'}else if(h==='vistainterface.com'||h.endsWith('.vistainterface.com')){k='v'}var n=false;try{var x=w.next;n=!!x&&typeof x==='object'&&typeof x.version==='string'}catch(e){}var c=false;try{c=typeof w.__VISTA_ANSWER_CALL==='function'}catch(e){}var q=false,a=false;try{var cn=w.connect;q=!!cn&&!!cn.core&&typeof cn.core==='object';a=!!cn&&!!cn.agent&&cn.agent.initialized===true}catch(e){}var s=null,t=null;try{var pf=w.performance,nv=pf.getEntriesByType('navigation');if(nv&&nv[0]&&typeof nv[0].responseStatus==='number'){s=nv[0].responseStatus}t=pf.getEntriesByType('resource').length}catch(e){}var o=null;try{o=w.navigator.onLine===true}catch(e){}return{k:k,n:n,c:c,q:q,a:a,r:String(w.document.readyState),s:s,t:t,o:o,p:(k==='p'||k==='v')?String(l.pathname).slice(0,200):''}}catch(e){return{k:'x'}}})()"#;

// ─── Pages and paths ─────────────────────────────────────────────────

/// What kind of page a URL (or a probe answer) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Platform,
    Vista,
    Blank,
    Error,
    Other,
}

impl PageKind {
    pub fn word(self) -> &'static str {
        match self {
            PageKind::Platform => "platform",
            PageKind::Vista => "vista",
            PageKind::Blank => "blank",
            PageKind::Error => "error",
            PageKind::Other => "other",
        }
    }
}

/// The shell's own bridge navigation (never a page load; rule 1f).
pub fn is_sentinel(url: &str) -> bool {
    url.starts_with(SENTINEL_PREFIX)
}

/// Empty or about: is Blank; any non-https scheme (chrome-error:, edge:,
/// http:, data:) or an unparseable string is Error; https on the Platform host
/// is Platform, on vistainterface.com or one of its subdomains Vista, and on
/// any other host Other.
pub fn url_kind(url: &str) -> PageKind {
    let url = url.trim();
    if url.is_empty() {
        return PageKind::Blank;
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return PageKind::Error;
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme == "about" {
        return PageKind::Blank;
    }
    if scheme != "https" {
        return PageKind::Error;
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return PageKind::Error;
    };
    let host = host_of(rest);
    if host.is_empty() {
        PageKind::Error
    } else if host == PLATFORM_HOST {
        PageKind::Platform
    } else if host == "vistainterface.com" || host.ends_with(".vistainterface.com") {
        PageKind::Vista
    } else {
        PageKind::Other
    }
}

/// The lower-cased host of "<authority>/<path>..." (no user info, no port).
fn host_of(after_slashes: &str) -> String {
    let authority = after_slashes
        .split(&['/', '?', '#'][..])
        .next()
        .unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    host_port
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The path of an absolute URL: after the authority, before any ? or #.
fn path_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or("");
    let rest = rest.split(&['?', '#'][..]).next().unwrap_or("");
    rest.split_once('/').map(|(_, p)| p).unwrap_or("")
}

/// A PHI-safe path template: no query or fragment; a UUID segment becomes
/// `:id` and an all-digit one `:n`; a long, odd-character or token-like one
/// `:x`; any other digit run `:n`. At most 8 segments, then `/...`.
pub fn path_template(path: &str) -> String {
    let path = path.split(&['?', '#'][..]).next().unwrap_or("");
    let mut out = String::new();
    for (i, seg) in path.split('/').filter(|s| !s.is_empty()).enumerate() {
        if i == 8 {
            out.push_str("/...");
            break;
        }
        out.push('/');
        out.push_str(&segment_template(seg));
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn segment_template(seg: &str) -> String {
    if is_uuid(seg) {
        return ":id".to_string();
    }
    if seg.chars().all(|c| c.is_ascii_digit()) {
        return ":n".to_string();
    }
    let len = seg.chars().count();
    let plain = seg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if len > 40 || !plain || (len >= 16 && seg.chars().any(|c| c.is_ascii_digit())) {
        return ":x".to_string();
    }
    let mut out = String::new();
    let mut in_digits = false;
    for c in seg.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push_str(":n");
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Where a navigation goes, for the log: `platform:/manage/:id/voice`,
/// `vista:/login`, `blank`, `error` or `other` (never a foreign host, never a
/// query string).
pub fn log_target(url: &str) -> String {
    match url_kind(url) {
        PageKind::Platform => format!("platform:{}", path_template(path_of(url))),
        PageKind::Vista => format!("vista:{}", path_template(path_of(url))),
        kind => kind.word().to_string(),
    }
}

// ─── Navigation results ──────────────────────────────────────────────

/// What a NavigationCompleted means. Every non-Ok class only pulls the next
/// probe forward; none rebuilds by itself (rule 1b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavClass {
    Ok,
    Server(u16),
    Network,
    Canceled,
    BlankAfterFailure,
    Other,
}

impl NavClass {
    /// A strong failure: the old document was replaced by an error page (or
    /// left blank), so nothing can live on in it.
    pub fn strong(self) -> bool {
        matches!(
            self,
            NavClass::Server(_) | NavClass::Network | NavClass::BlankAfterFailure
        )
    }

    pub fn word(self) -> String {
        match self {
            NavClass::Ok => "ok".to_string(),
            NavClass::Server(code) => format!("server-{code}"),
            NavClass::Network => "network".to_string(),
            NavClass::Canceled => "canceled".to_string(),
            NavClass::BlankAfterFailure => "blank".to_string(),
            NavClass::Other => "other".to_string(),
        }
    }
}

/// `ok` = IsSuccess, `err` = WebErrorStatus, `http` = HttpStatusCode (None on
/// a runtime without it), `source` = the kind of the web view's Source after
/// the navigation. CONNECTION_ABORTED and OPERATION_CANCELED also happen while
/// the old page lives on (a newer navigation, "Leave site?" answered Stay,
/// window.stop()), so they are only Canceled.
pub fn classify_nav(ok: bool, err: i32, http: Option<i32>, source: Option<PageKind>) -> NavClass {
    if let Some(code) = http {
        if (500..=599).contains(&code) {
            return NavClass::Server(code as u16);
        }
    }
    if ok {
        return NavClass::Ok;
    }
    if source == Some(PageKind::Blank) {
        return NavClass::BlankAfterFailure;
    }
    let no_response = matches!(http, None | Some(0));
    match err {
        // SERVER_UNREACHABLE, TIMEOUT, ERROR_HTTP_INVALID_SERVER_RESPONSE,
        // CONNECTION_RESET, DISCONNECTED, CANNOT_CONNECT, HOST_NAME_NOT_RESOLVED
        6 | 7 | 8 | 10 | 11 | 12 | 13 if no_response => NavClass::Network,
        // CONNECTION_ABORTED, OPERATION_CANCELED
        9 | 14 => NavClass::Canceled,
        _ => NavClass::Other,
    }
}

/// COREWEBVIEW2_WEB_ERROR_STATUS by number (webview2-com-sys 0.38.2).
pub fn web_error_name(code: i32) -> &'static str {
    match code {
        0 => "UNKNOWN",
        1 => "CERTIFICATE_COMMON_NAME_IS_INCORRECT",
        2 => "CERTIFICATE_EXPIRED",
        3 => "CLIENT_CERTIFICATE_CONTAINS_ERRORS",
        4 => "CERTIFICATE_REVOKED",
        5 => "CERTIFICATE_IS_INVALID",
        6 => "SERVER_UNREACHABLE",
        7 => "TIMEOUT",
        8 => "ERROR_HTTP_INVALID_SERVER_RESPONSE",
        9 => "CONNECTION_ABORTED",
        10 => "CONNECTION_RESET",
        11 => "DISCONNECTED",
        12 => "CANNOT_CONNECT",
        13 => "HOST_NAME_NOT_RESOLVED",
        14 => "OPERATION_CANCELED",
        15 => "REDIRECT_FAILED",
        16 => "UNEXPECTED_ERROR",
        17 => "VALID_AUTHENTICATION_CREDENTIALS_REQUIRED",
        18 => "VALID_PROXY_AUTHENTICATION_REQUIRED",
        _ => "OTHER",
    }
}

/// COREWEBVIEW2_PROCESS_FAILED_KIND by number (webview2-com-sys 0.38.2).
pub fn process_kind_name(kind: i32) -> &'static str {
    match kind {
        0 => "BROWSER_PROCESS_EXITED",
        1 => "RENDER_PROCESS_EXITED",
        2 => "RENDER_PROCESS_UNRESPONSIVE",
        3 => "FRAME_RENDER_PROCESS_EXITED",
        4 => "UTILITY_PROCESS_EXITED",
        5 => "SANDBOX_HELPER_PROCESS_EXITED",
        6 => "GPU_PROCESS_EXITED",
        7 => "PPAPI_PLUGIN_PROCESS_EXITED",
        8 => "PPAPI_BROKER_PROCESS_EXITED",
        9 => "UNKNOWN_PROCESS_EXITED",
        _ => "OTHER",
    }
}

// ─── The probe answer and the decisive judgement ─────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    Loading,
    Interactive,
    Complete,
    Unknown,
}

impl Ready {
    pub fn word(self) -> &'static str {
        match self {
            Ready::Loading => "loading",
            Ready::Interactive => "interactive",
            Ready::Complete => "complete",
            Ready::Unknown => "unknown",
        }
    }
}

/// A parsed probe answer (see PROBE_JS).
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReport {
    pub kind: PageKind,
    /// `n`: the Next.js runtime started.
    pub next: bool,
    /// `c`: the voice page's softphone hook exists.
    pub hook: bool,
    /// `q`: Amazon's Streams library is loaded.
    pub streams: bool,
    /// `a`: the softphone's agent arrived (the softphone is up).
    pub agent: bool,
    pub ready: Ready,
    pub status: Option<u16>,
    pub resources: Option<u32>,
    pub online: Option<bool>,
    pub path_tpl: String,
}

impl ProbeReport {
    pub fn is_vista(&self) -> bool {
        matches!(self.kind, PageKind::Platform | PageKind::Vista)
    }

    /// The page for the log: its kind, plus the path template on a VISTA host.
    pub fn page_word(&self) -> String {
        if self.is_vista() {
            format!("{}:{}", self.kind.word(), self.path_tpl)
        } else {
            self.kind.word().to_string()
        }
    }
}

/// The probe's JSON (ExecuteScript's serialised result). None for anything
/// that is not a well-formed answer: "null", an `x` (the probe threw), garbage.
/// A None counts as "no answer", never as dead.
pub fn parse_probe(json: &str) -> Option<ProbeReport> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = value.as_object()?;
    let kind = match obj.get("k").and_then(|x| x.as_str())? {
        "p" => PageKind::Platform,
        "v" => PageKind::Vista,
        "b" => PageKind::Blank,
        "e" => PageKind::Error,
        "o" => PageKind::Other,
        _ => return None,
    };
    let flag = |key: &str| obj.get(key).and_then(|x| x.as_bool()).unwrap_or(false);
    let ready = match obj.get("r").and_then(|x| x.as_str()) {
        Some("complete") => Ready::Complete,
        Some("interactive") => Ready::Interactive,
        Some("loading") => Ready::Loading,
        _ => Ready::Unknown,
    };
    let status = obj
        .get("s")
        .and_then(|x| x.as_u64())
        .filter(|s| (1..=999).contains(s))
        .map(|s| s as u16);
    let resources = obj
        .get("t")
        .and_then(|x| x.as_u64())
        .map(|t| t.min(u64::from(u32::MAX)) as u32);
    let online = obj.get("o").and_then(|x| x.as_bool());
    let path_tpl = match kind {
        PageKind::Platform | PageKind::Vista => {
            path_template(obj.get("p").and_then(|x| x.as_str()).unwrap_or(""))
        }
        _ => String::new(),
    };
    Some(ProbeReport {
        kind,
        next: flag("n"),
        hook: flag("c"),
        streams: flag("q"),
        agent: flag("a"),
        ready,
        status,
        resources,
        online,
        path_tpl,
    })
}

/// Why a page counts as dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadReason {
    /// about:blank past the grace.
    Blank,
    /// Chromium's own error page.
    ErrorPage,
    /// A VISTA document with no app running in it (the load balancer's 502
    /// page is exactly this, with status 502).
    NoApp(Option<u16>),
    /// A VISTA document that stopped loading half-way.
    StillLoading,
    /// Next's built-in error page: the marker, but a 5xx document.
    ServerError(u16),
    /// The newest navigation committed a failure (this class) and the page
    /// has not answered a probe since.
    Silent(NavClass),
    /// The web view never committed a document and its load is long overdue.
    NeverLoaded,
}

impl DeadReason {
    pub fn word(self) -> String {
        match self {
            DeadReason::Blank => "blank".to_string(),
            DeadReason::ErrorPage => "error-page".to_string(),
            DeadReason::NoApp(Some(s)) => format!("no-app:{s}"),
            DeadReason::NoApp(None) => "no-app:-".to_string(),
            DeadReason::StillLoading => "still-loading".to_string(),
            DeadReason::ServerError(s) => format!("server-error:{s}"),
            DeadReason::Silent(class) => format!("silent:{}", class.word()),
            DeadReason::NeverLoaded => "never-loaded".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Alive,
    Loading,
    Dead(DeadReason),
}

fn verdict_word(v: Verdict) -> String {
    match v {
        Verdict::Alive => "alive".to_string(),
        Verdict::Loading => "loading".to_string(),
        Verdict::Dead(r) => format!("dead:{}", r.word()),
    }
}

/// The decisive rule. `since_nav` = time since the last navigation start (or
/// the web view's creation), `since_content` = time since that navigation's
/// document began loading, `in_flight` = started and not completed.
///
/// A VISTA page with ANY sign of a running app (the voice page's hook,
/// Streams, or Next's runtime on a document that is not 5xx) is alive,
/// whatever its path: the page's own JavaScript handles softphone trouble.
/// A foreign https page is left alone. Nothing is judged inside the grace.
pub fn judge_probe(
    r: &ProbeReport,
    since_nav: Duration,
    since_content: Option<Duration>,
    in_flight: bool,
) -> Verdict {
    let vista = r.is_vista();
    let server_error = r.status.is_some_and(|s| (500..=599).contains(&s));
    if vista && (r.hook || r.streams) {
        return Verdict::Alive;
    }
    if vista && r.next && !server_error {
        return Verdict::Alive;
    }
    if r.kind == PageKind::Other {
        return Verdict::Alive;
    }
    if since_nav < NAV_GRACE || (in_flight && since_nav < IN_FLIGHT_LIMIT) {
        return Verdict::Loading;
    }
    if vista && r.next {
        return Verdict::Dead(DeadReason::ServerError(r.status.unwrap_or(500)));
    }
    match r.kind {
        PageKind::Blank => return Verdict::Dead(DeadReason::Blank),
        PageKind::Error => return Verdict::Dead(DeadReason::ErrorPage),
        _ => {}
    }
    // A VISTA document with no app running in it.
    if r.ready == Ready::Complete {
        return Verdict::Dead(DeadReason::NoApp(r.status));
    }
    let load_clock = since_content.unwrap_or(since_nav);
    if load_clock >= LOAD_LIMIT {
        Verdict::Dead(DeadReason::StillLoading)
    } else {
        Verdict::Loading
    }
}

// ─── Triggers, the call-flag hold and restart evidence ───────────────

/// Why a rebuild happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Dead(DeadReason),
    /// Probes went unanswered for UNRESPONSIVE_LIMIT (the page may be alive
    /// behind a dialog: never with a call flagged, never a restart).
    Unresponsive,
    /// The renderer (or the whole browser process) exited.
    RendererGone(i32),
    /// An owner, but no Phone tab at all.
    Missing,
    /// The page's own reconnect sentinel (3:30 AM refresh, Reconnect, ...).
    PageAsked,
}

impl Trigger {
    /// No live page can be left in the tab, so a stale call flag may go.
    pub fn definite(self) -> bool {
        matches!(
            self,
            Trigger::Dead(_) | Trigger::RendererGone(_) | Trigger::Missing
        )
    }

    /// Only a confirmed dead tab ever sets the restart pending (never a page
    /// that asked, one that stopped answering, or a missing tab).
    pub fn arms_restart(self) -> bool {
        matches!(self, Trigger::Dead(_) | Trigger::RendererGone(_))
    }

    pub fn word(self) -> String {
        match self {
            Trigger::Dead(r) => format!("dead:{}", r.word()),
            Trigger::Unresponsive => "unresponsive".to_string(),
            Trigger::RendererGone(kind) => format!("renderer-gone:{}", process_kind_name(kind)),
            Trigger::Missing => "missing".to_string(),
            Trigger::PageAsked => "page-asked".to_string(),
        }
    }
}

/// A death no live call could survive. While the call flag is up, only these
/// clear it and rebuild; anything else is held (the flag may be real).
pub fn unambiguous(t: Trigger) -> bool {
    match t {
        Trigger::Dead(r) => match r {
            DeadReason::Blank
            | DeadReason::ErrorPage
            | DeadReason::ServerError(_)
            | DeadReason::Silent(_)
            | DeadReason::NeverLoaded => true,
            DeadReason::NoApp(status) => status.is_some_and(|s| s >= 400),
            DeadReason::StillLoading => false,
        },
        Trigger::RendererGone(_) | Trigger::Missing => true,
        Trigger::Unresponsive | Trigger::PageAsked => false,
    }
}

/// What a dead tab says about where the trouble is, for the restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The app's own engine (a cut-off load, a blank tab, a dead renderer): a
    /// fresh process is the cure.
    Engine,
    /// The server answered with an error.
    Server,
    /// The network (or the server) could not be reached.
    Network,
    /// The document arrived but no app ran in it.
    Unclear,
}

impl Evidence {
    pub fn word(self) -> &'static str {
        match self {
            Evidence::Engine => "engine",
            Evidence::Server => "server",
            Evidence::Network => "network",
            Evidence::Unclear => "unclear",
        }
    }
}

/// Server, network and unclear evidence hold a restart until a page has
/// probed alive since (a relaunch into an outage would leave a dead Platform
/// tab and no Phone tab, with nothing to bring them back).
pub fn restart_evidence(t: Trigger, last_class: Option<NavClass>, online: Option<bool>) -> Evidence {
    if online == Some(false) {
        return Evidence::Network;
    }
    match last_class {
        Some(NavClass::Server(_)) => return Evidence::Server,
        Some(NavClass::Network) => return Evidence::Network,
        _ => {}
    }
    match t {
        Trigger::Dead(r) => match r {
            DeadReason::Blank | DeadReason::NeverLoaded | DeadReason::StillLoading => {
                Evidence::Engine
            }
            DeadReason::ErrorPage => Evidence::Network,
            DeadReason::ServerError(_) => Evidence::Server,
            DeadReason::NoApp(Some(s)) if s >= 400 => Evidence::Server,
            DeadReason::NoApp(_) => Evidence::Unclear,
            DeadReason::Silent(class) => match class {
                NavClass::Server(_) => Evidence::Server,
                NavClass::BlankAfterFailure => Evidence::Engine,
                _ => Evidence::Network,
            },
        },
        Trigger::RendererGone(_) => Evidence::Engine,
        Trigger::Unresponsive | Trigger::Missing | Trigger::PageAsked => Evidence::Unclear,
    }
}

/// Rebuilds already started in this dead streak → the wait before the next:
/// 10 s, 30 s, 60 s, 2 min, then every 5 min.
pub fn backoff_delay(attempts: u32) -> Duration {
    Duration::from_secs(match attempts {
        0 => 10,
        1 => 30,
        2 => 60,
        3 => 120,
        _ => 300,
    })
}

// ─── Owners, URLs, time ──────────────────────────────────────────────

/// A VISTA id as the web app writes it (the same rule as lib.rs is_vista_id).
pub fn is_vista_id(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// "<companyId>/<userId>" → the two ids, when both are VISTA ids.
pub fn split_owner(owner: &str) -> Option<(&str, &str)> {
    let (company, user) = owner.split_once('/')?;
    if is_vista_id(company) && is_vista_id(user) {
        Some((company, user))
    } else {
        None
    }
}

/// The always-on Phone tab's address for its owner.
pub fn voice_url_for_owner(owner: &str) -> Option<String> {
    let (company, _) = split_owner(owner)?;
    Some(format!("https://{PLATFORM_HOST}/manage/{company}/voice"))
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for a Unix time in milliseconds.
pub fn utc_stamp(unix_ms: u128) -> String {
    let ms = (unix_ms % 1000) as u32;
    let secs = unix_ms / 1000;
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60
    )
}

/// Days since 1970-01-01 → (year, month, day) (H. Hinnant's civil_from_days).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Windows idle time from two 32-bit tick counts (GetTickCount and
/// LASTINPUTINFO.dwTime). Both wrap every 49.7 days, so only wrapping math is
/// right: a 64-bit difference would make the PC look idle forever.
pub fn idle_ms(now_tick: u32, last_input_tick: u32) -> u32 {
    now_tick.wrapping_sub(last_input_tick)
}

// ─── The restart record (persisted) and the hand-off ─────────────────

/// Why the cooldown or the cap holds a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartHold {
    Cooldown { since_last_s: u64 },
    Cap { count: usize },
}

/// What reading the restart record gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRead {
    Missing,
    Text(String),
    Unreadable,
}

/// The record's restart times (Unix ms), and whether the file must be
/// rewritten. A file that can't be read or parsed counts as "restarted just
/// now" (the cautious answer), and is rewritten so it heals.
pub fn history_from_read(read: &HistoryRead, now_ms: u64) -> (Vec<u64>, bool) {
    match read {
        HistoryRead::Missing => (Vec::new(), false),
        HistoryRead::Text(text) => match parse_history(text) {
            Some(list) => (list, false),
            None => (vec![now_ms], true),
        },
        HistoryRead::Unreadable => (vec![now_ms], true),
    }
}

pub fn parse_history(text: &str) -> Option<Vec<u64>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out = Vec::new();
    for item in value.as_array()? {
        out.push(item.as_u64()?);
    }
    Some(out)
}

pub fn history_json(list: &[u64]) -> String {
    serde_json::to_string(list).unwrap_or_else(|_| "[]".to_string())
}

/// Times in the future (the clock moved back) count as now; times older than
/// the window are dropped. Returns the list (sorted) and whether it changed.
pub fn normalize_history(raw: &[u64], now_ms: u64) -> (Vec<u64>, bool) {
    let mut changed = false;
    let mut out = Vec::new();
    for &t in raw {
        let t = if t > now_ms {
            changed = true;
            now_ms
        } else {
            t
        };
        if now_ms.saturating_sub(t) >= RESTART_WINDOW_MS {
            changed = true;
            continue;
        }
        out.push(t);
    }
    out.sort_unstable();
    (out, changed)
}

/// May VISTA restart itself now? Ok carries the record to write (with `now`
/// added): the caller must write it, and verify it, before restarting.
pub fn restart_decision(history: &[u64], now_ms: u64) -> Result<Vec<u64>, RestartHold> {
    let (list, _) = normalize_history(history, now_ms);
    if let Some(&last) = list.iter().max() {
        let since = now_ms.saturating_sub(last);
        if since < RESTART_COOLDOWN_MS {
            return Err(RestartHold::Cooldown {
                since_last_s: since / 1000,
            });
        }
    }
    if list.len() >= RESTART_CAP {
        return Err(RestartHold::Cap { count: list.len() });
    }
    let mut next = list;
    next.push(now_ms);
    Ok(next)
}

/// The one-shot hand-off a restart leaves for the next copy: whose Phone tab
/// to reopen (staff ids only), and when.
pub fn handoff_json(owner: &str, at_ms: u64) -> String {
    serde_json::json!({ "owner": owner, "at": at_ms }).to_string()
}

/// The hand-off's (company, user), only when it is fresh and well-formed.
pub fn parse_handoff(text: &str, now_ms: u64) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let owner = value.get("owner")?.as_str()?;
    let at = value.get("at")?.as_u64()?;
    if at > now_ms.saturating_add(5_000) || now_ms.saturating_sub(at) > HANDOFF_MAX_AGE_MS {
        return None;
    }
    let (company, user) = split_owner(owner)?;
    Some((company.to_ascii_lowercase(), user.to_ascii_lowercase()))
}

// ─── Events, snapshot, actions ───────────────────────────────────────

/// NavigationStarting (the raw address is already reduced to a template).
#[derive(Debug, Clone)]
pub struct NavStart {
    pub id: u64,
    pub at: Instant,
    pub sentinel: bool,
    pub target: String,
    pub redirected: bool,
}

/// NavigationCompleted.
#[derive(Debug, Clone)]
pub struct NavDone {
    pub id: u64,
    pub at: Instant,
    pub ok: bool,
    pub err: i32,
    pub http: Option<i32>,
    pub source: Option<PageKind>,
}

/// ContentLoading: a document committed (`error_page`: Chromium's own).
#[derive(Debug, Clone)]
pub struct ContentEvt {
    pub id: u64,
    pub at: Instant,
    pub error_page: bool,
}

/// What came back from ExecuteScript.
#[derive(Debug, Clone)]
pub enum ProbeOutcome {
    Json(String),
    Failed(i32),
    NoWebview,
}

/// What the tick reads from TabState and the flags.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub exiting: bool,
    /// TabState.phone_owner.
    pub owner: Option<String>,
    /// The first Phone tab's label.
    pub phone: Option<String>,
    pub phone_pinned: bool,
    pub phone_has_webview: bool,
    pub call_active: bool,
}

/// What the rules ask the shell to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Probe { label: String, gen: u64, id: u64 },
    RunRebuild { id: u64, trigger: Trigger },
    ClearStaleCall,
    FinishSignOut,
    /// Check whether a restart is safe right now, and restart if it is.
    TryRestart,
    Log(String),
}

fn log_action(msg: String) -> Action {
    Action::Log(msg)
}

// ─── The state machine ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Waiting { at: Instant },
    Closing { since: Instant },
}

#[derive(Debug, Clone)]
struct Rebuild {
    /// The ticket: the queued open step runs only while this is current.
    id: u64,
    trigger: Trigger,
    stage: Stage,
    expedited: bool,
}

#[derive(Debug, Clone, Copy)]
struct NavRec {
    id: u64,
    at: Instant,
    sentinel: bool,
}

#[derive(Debug, Clone, Copy)]
struct FailedCommit {
    id: u64,
    class: NavClass,
}

/// The Phone web view being watched (the latest one created).
#[derive(Debug, Clone)]
struct Cur {
    label: String,
    gen: u64,
    /// It is the pinned, always-on Phone tab (refreshed every tick).
    watched: bool,
    /// Creation time until a real NavigationStarting arrives.
    nav_started: Instant,
    /// Some(0) = the initial load (synthetic) until anything completes.
    nav_in_flight: Option<u64>,
    newest_real: Option<u64>,
    navs: VecDeque<NavRec>,
    recent_starts: VecDeque<Instant>,
    storm_logged: bool,
    /// Any document committed in this web view (ContentLoading, or a load OK).
    committed_any: bool,
    /// When the newest navigation's document began loading.
    content_at: Option<Instant>,
    /// The newest navigation committed a failure (error page, 5xx, blank).
    failed_commit: Option<FailedCommit>,
    last_class: Option<NavClass>,
    next_probe_at: Instant,
    /// The probe in flight: id, sent.
    probe: Option<(u64, Instant)>,
    /// The last probe that timed out (its late answer still counts as life).
    late_probe: Option<u64>,
    strikes: u32,
    first_strike_at: Option<Instant>,
    strike_resources: Option<u32>,
    no_answer_since: Option<Instant>,
    hold_logged: bool,
}

impl Cur {
    fn new(label: String, gen: u64, now: Instant, watched: bool) -> Cur {
        Cur {
            label,
            gen,
            watched,
            nav_started: now,
            nav_in_flight: Some(0),
            newest_real: None,
            navs: VecDeque::new(),
            recent_starts: VecDeque::new(),
            storm_logged: false,
            committed_any: false,
            content_at: None,
            failed_commit: None,
            last_class: None,
            next_probe_at: now + NAV_GRACE,
            probe: None,
            late_probe: None,
            strikes: 0,
            first_strike_at: None,
            strike_resources: None,
            no_answer_since: None,
            hold_logged: false,
        }
    }

    fn reset_strikes(&mut self) {
        self.strikes = 0;
        self.first_strike_at = None;
        self.strike_resources = None;
    }

    fn storm(&self) -> bool {
        self.recent_starts.len() >= STORM_STARTS
    }

    /// `next_probe_at` moves earlier only, never inside the grace.
    fn probe_soon(&mut self, now: Instant) {
        let earliest = self.nav_started + NAV_GRACE;
        let soon = if now > earliest { now } else { earliest };
        if soon < self.next_probe_at {
            self.next_probe_at = soon;
        }
    }
}

fn judge_cur(cur: &Cur, report: &ProbeReport, now: Instant) -> Verdict {
    let since_nav = now.saturating_duration_since(cur.nav_started);
    let since_content = cur.content_at.map(|t| now.saturating_duration_since(t));
    judge_probe(report, since_nav, since_content, cur.nav_in_flight.is_some())
}

/// A restart waiting until it is safe.
#[derive(Debug, Clone)]
struct RestartPending {
    armed_at: Instant,
    trigger: Trigger,
    evidence: Evidence,
    /// A page probed alive since: the server answers.
    server_ok: bool,
    min_pending: Duration,
    last_check: Option<Instant>,
    last_hold: Option<(String, Instant)>,
}

fn note_hold(r: &mut RestartPending, now: Instant, word: &str, detail: &str) -> Option<String> {
    let due = match &r.last_hold {
        Some((w, t)) => w != word || now.saturating_duration_since(*t) >= HOLD_LOG_EVERY,
        None => true,
    };
    if !due {
        return None;
    }
    r.last_hold = Some((word.to_string(), now));
    Some(format!("restart held reason={word}{detail}"))
}

/// The watchdog's whole state. Only the main thread touches it.
pub struct Watch {
    cur: Option<Cur>,
    /// "<company>/<user>" the web app last asked for (ensure_phone_tab).
    wanted: Option<String>,
    /// Sign-out asked: never rebuild or re-create until the next ensure.
    signout_pending: bool,
    /// The ONE rebuild in flight.
    rebuild: Option<Rebuild>,
    /// Rebuilds started in this dead streak (drives the backoff).
    attempts: u32,
    alive_since: Option<Instant>,
    dead_since: Option<Instant>,
    missing_since: Option<Instant>,
    last_online: Option<bool>,
    net_back_at: Option<Instant>,
    stuck_logged: bool,
    /// Probe ids and rebuild tickets.
    seq: u64,
    last_probe_word: String,
    last_probe_log: Option<Instant>,
    idle_logged: bool,
    unpinned_logged: bool,
    restart: Option<RestartPending>,
    /// BROWSER_PROCESS_EXITED: every web view is gone; only a restart helps.
    engine_gone: bool,
}

impl Default for Watch {
    fn default() -> Self {
        Watch::new()
    }
}

impl Watch {
    pub const fn new() -> Watch {
        Watch {
            cur: None,
            wanted: None,
            signout_pending: false,
            rebuild: None,
            attempts: 0,
            alive_since: None,
            dead_since: None,
            missing_since: None,
            last_online: None,
            net_back_at: None,
            stuck_logged: false,
            seq: 0,
            last_probe_word: String::new(),
            last_probe_log: None,
            idle_logged: false,
            unpinned_logged: false,
            restart: None,
            engine_gone: false,
        }
    }

    pub fn wanted(&self) -> Option<String> {
        self.wanted.clone()
    }

    /// The pending restart's reason, if one is pending.
    pub fn restart_reason(&self) -> Option<String> {
        self.restart.as_ref().map(|r| r.trigger.word())
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn probe_line(&mut self, now: Instant, word: &str, line: String) -> Option<Action> {
        let every = if word == "alive" { HEARTBEAT_LOG } else { REPEAT_LOG };
        let due = word != self.last_probe_word
            || self
                .last_probe_log
                .map_or(true, |t| now.saturating_duration_since(t) >= every);
        if !due {
            return None;
        }
        self.last_probe_word = word.to_string();
        self.last_probe_log = Some(now);
        Some(log_action(line))
    }

    // ── Web views ──────────────────────────────────────────────────

    /// A Phone web view was created (attach, inside open_tab_ex).
    pub fn on_webview_created(&mut self, now: Instant, label: &str, gen: u64, watched: bool) -> Vec<Action> {
        let mut out = vec![log_action(format!("webview create gen={gen} tab={label}"))];
        self.cur = Some(Cur::new(label.to_string(), gen, now, watched));
        self.missing_since = None;
        self.engine_gone = false;
        if let Some(rb) = self.rebuild.take() {
            let word = match rb.stage {
                Stage::Closing { .. } => "rebuild opened",
                Stage::Waiting { .. } => "rebuild superseded",
            };
            out.push(log_action(format!("{word} id={} trigger={}", rb.id, rb.trigger.word())));
        }
        out
    }

    /// The web view is going away (close_tab_impl): its events are stale.
    pub fn on_webview_closed(&mut self, label: &str) -> Vec<Action> {
        let gen = match &self.cur {
            Some(c) if c.label == label => c.gen,
            _ => return Vec::new(),
        };
        self.cur = None;
        vec![log_action(format!("webview close gen={gen} tab={label}"))]
    }

    // ── Page loads ─────────────────────────────────────────────────

    pub fn on_nav_starting(&mut self, gen: u64, s: NavStart) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(cur) = self.cur.as_mut() else {
            return out;
        };
        if cur.gen != gen {
            return out;
        }
        if cur.navs.len() >= NAV_MEMORY {
            cur.navs.pop_front();
        }
        cur.navs.push_back(NavRec {
            id: s.id,
            at: s.at,
            sentinel: s.sentinel,
        });
        if s.sentinel {
            // The shell's own bridge navigation: never counts (rule 1f).
            return out;
        }
        while let Some(&t) = cur.recent_starts.front() {
            if s.at.saturating_duration_since(t) >= STORM_WINDOW {
                cur.recent_starts.pop_front();
            } else {
                break;
            }
        }
        cur.recent_starts.push_back(s.at);
        cur.newest_real = Some(s.id);
        cur.nav_in_flight = Some(s.id);
        cur.content_at = None;
        cur.failed_commit = None;
        if cur.storm() {
            // An error page that keeps reloading, a redirect loop: the grace
            // is no longer renewed, so a dead page still gets judged.
            if !cur.storm_logged {
                cur.storm_logged = true;
                out.push(log_action(format!("navigation storm gen={gen}")));
            }
        } else {
            cur.nav_started = s.at;
            cur.reset_strikes();
            cur.no_answer_since = None;
            cur.next_probe_at = s.at + NAV_GRACE;
        }
        out.push(log_action(format!(
            "nav start gen={gen} nav={} to={} redirect={}",
            s.id, s.target, s.redirected as u8
        )));
        out
    }

    pub fn on_content_loading(&mut self, now: Instant, gen: u64, c: ContentEvt) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(cur) = self.cur.as_mut() else {
            return out;
        };
        if cur.gen != gen {
            return out;
        }
        if cur.navs.iter().any(|n| n.id == c.id && n.sentinel) {
            return out;
        }
        cur.committed_any = true;
        let newest = cur.newest_real.map_or(true, |id| id == c.id);
        if newest {
            cur.content_at = Some(c.at);
            if c.error_page {
                // The old document is gone: an error page committed in its place.
                if cur.failed_commit.map_or(true, |f| f.id != c.id) {
                    cur.failed_commit = Some(FailedCommit {
                        id: c.id,
                        class: NavClass::Network,
                    });
                }
                cur.probe_soon(now);
            }
        }
        out.push(log_action(format!(
            "content gen={gen} nav={} error-page={}",
            c.id, c.error_page as u8
        )));
        out
    }

    pub fn on_nav_completed(&mut self, now: Instant, gen: u64, d: NavDone) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(cur) = self.cur.as_mut() else {
            return out;
        };
        if cur.gen != gen {
            return out;
        }
        let rec = cur.navs.iter().find(|n| n.id == d.id).copied();
        if rec.is_some_and(|r| r.sentinel) {
            return out;
        }
        if cur.nav_in_flight == Some(d.id) || cur.nav_in_flight == Some(0) {
            cur.nav_in_flight = None;
        }
        let class = classify_nav(d.ok, d.err, d.http, d.source);
        let newest = cur.newest_real.map_or(true, |id| id == d.id);
        if newest {
            cur.last_class = Some(class);
            if class == NavClass::Ok {
                cur.committed_any = true;
                cur.failed_commit = None;
            } else if class.strong() {
                cur.failed_commit = Some(FailedCommit { id: d.id, class });
            }
        }
        if class != NavClass::Ok {
            // Only a reason to look soon, never a rebuild by itself (rule 1b).
            cur.probe_soon(now);
        }
        let ms = rec.map_or_else(
            || "-".to_string(),
            |r| d.at.saturating_duration_since(r.at).as_millis().to_string(),
        );
        let http = d.http.map_or_else(|| "-".to_string(), |h| h.to_string());
        let source = d.source.map_or("-", |k| k.word());
        out.push(log_action(format!(
            "nav done gen={gen} nav={} ok={} http={http} err={} ms={ms} class={} source={source}",
            d.id,
            d.ok as u8,
            web_error_name(d.err),
            class.word()
        )));
        out
    }

    pub fn on_process_failed(&mut self, now: Instant, gen: u64, kind: i32, call_active: bool) -> Vec<Action> {
        let watched = match &self.cur {
            Some(c) if c.gen == gen => c.watched,
            _ => return Vec::new(),
        };
        let mut out = vec![log_action(format!(
            "process-failed gen={gen} kind={}",
            process_kind_name(kind)
        ))];
        match kind {
            // The renderer, or the whole browser process, exited: the page is
            // gone, every frame with it (the softphone's included).
            0 | 1 => {
                if !watched {
                    return out;
                }
                if kind == 0 {
                    self.engine_gone = true;
                }
                out.extend(self.verdict_dead(now, Trigger::RendererGone(kind), call_active));
            }
            // Unresponsive: probe right away, no verdict.
            2 => {
                if let Some(cur) = self.cur.as_mut() {
                    cur.next_probe_at = now;
                }
            }
            // A frame's renderer (the softphone frame, say) or a helper
            // process: the page is alive and handles its own frame.
            _ => {}
        }
        out
    }

    // ── Probes ─────────────────────────────────────────────────────

    pub fn on_probe_answer(
        &mut self,
        now: Instant,
        gen: u64,
        id: u64,
        outcome: ProbeOutcome,
        call_active: bool,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        let (current, late, sent_at) = match &self.cur {
            Some(c) if c.gen == gen => {
                let current = c.probe.filter(|p| p.0 == id);
                (current.is_some(), c.late_probe == Some(id), current.map(|p| p.1))
            }
            _ => return out,
        };
        if !current && !late {
            return out;
        }
        let report = match &outcome {
            ProbeOutcome::Json(json) => parse_probe(json),
            _ => None,
        };
        if late {
            // A late answer proves the renderer still answers. It is never
            // dead evidence; an alive one counts as alive.
            let verdict = match (&self.cur, &report) {
                (Some(c), Some(r)) => Some(judge_cur(c, r, now)),
                _ => None,
            };
            if let Some(cur) = self.cur.as_mut() {
                cur.late_probe = None;
                cur.no_answer_since = None;
                cur.hold_logged = false;
            }
            out.push(log_action(format!(
                "probe late gen={gen} id={id} answered={}",
                report.is_some() as u8
            )));
            if verdict == Some(Verdict::Alive) {
                out.extend(self.on_alive(now));
            }
            return out;
        }
        let sent_at = sent_at.unwrap_or(now);
        if let Some(cur) = self.cur.as_mut() {
            cur.probe = None;
        }
        let Some(report) = report else {
            let why = match &outcome {
                ProbeOutcome::Json(_) => "garbled".to_string(),
                ProbeOutcome::Failed(hr) => format!("failed:{:#010x}", *hr as u32),
                ProbeOutcome::NoWebview => "no-webview".to_string(),
            };
            out.extend(self.no_answer(now, call_active, &why, id));
            return out;
        };
        let mut flipped = false;
        if let Some(online) = report.online {
            if self.last_online == Some(false) && online {
                self.net_back_at = Some(now);
                flipped = true;
                out.push(log_action("network back".to_string()));
            }
            self.last_online = Some(online);
        }
        let (verdict, strikes) = match &self.cur {
            Some(c) => (judge_cur(c, &report, now), c.strikes),
            None => return out,
        };
        let word = verdict_word(verdict);
        let line = format!(
            "probe gen={gen} id={id} verdict={word} page={} next={} hook={} streams={} agent={} ready={} status={} online={} res={} strikes={strikes}",
            report.page_word(),
            report.next as u8,
            report.hook as u8,
            report.streams as u8,
            report.agent as u8,
            report.ready.word(),
            report.status.map_or_else(|| "-".to_string(), |s| s.to_string()),
            report.online.map_or("-", |o| if o { "1" } else { "0" }),
            report.resources.map_or_else(|| "-".to_string(), |t| t.to_string()),
        );
        if let Some(a) = self.probe_line(now, &word, line) {
            out.push(a);
        }
        // The softphone's agent arrived: the softphone works, no restart needed.
        if report.agent && report.is_vista() {
            out.extend(self.restart_ready("agent", sent_at));
        }
        match verdict {
            Verdict::Alive => out.extend(self.on_alive(now)),
            Verdict::Loading => {
                if let Some(cur) = self.cur.as_mut() {
                    cur.next_probe_at = now + LOADING_RETRY;
                }
            }
            Verdict::Dead(reason) => {
                out.extend(self.on_dead_answer(now, reason, &report, call_active, flipped))
            }
        }
        out
    }

    fn on_alive(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(cur) = self.cur.as_mut() {
            cur.reset_strikes();
            cur.no_answer_since = None;
            cur.hold_logged = false;
            cur.next_probe_at = now + PERIOD;
        }
        let cancel = match &self.rebuild {
            Some(rb) if matches!(rb.stage, Stage::Waiting { .. }) && rb.trigger != Trigger::PageAsked => {
                Some(rb.id)
            }
            _ => None,
        };
        if let Some(id) = cancel {
            self.rebuild = None;
            out.push(log_action(format!("came back by itself id={id}")));
        }
        if let Some(t) = self.dead_since.take() {
            out.push(log_action(format!(
                "recovered after={} dead-for={}s",
                self.attempts,
                now.saturating_duration_since(t).as_secs()
            )));
        }
        if self.alive_since.is_none() {
            self.alive_since = Some(now);
        }
        let stable = self
            .alive_since
            .is_some_and(|t| now.saturating_duration_since(t) >= STABLE_AFTER);
        if self.attempts > 0 && stable {
            self.attempts = 0;
            self.stuck_logged = false;
            out.push(log_action("backoff reset".to_string()));
        }
        if let Some(r) = self.restart.as_mut() {
            if !r.server_ok {
                r.server_ok = true;
                out.push(log_action("restart pending: the server answers".to_string()));
            }
        }
        out
    }

    fn on_dead_answer(
        &mut self,
        now: Instant,
        reason: DeadReason,
        report: &ProbeReport,
        call_active: bool,
        flipped: bool,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        self.alive_since = None;
        if self.rebuild.is_some() {
            if let Some(cur) = self.cur.as_mut() {
                cur.next_probe_at = now + WAIT_PROBE;
            }
            if flipped {
                if let Some(rb) = self.rebuild.as_mut() {
                    if let Stage::Waiting { at } = rb.stage {
                        if !rb.expedited {
                            let soon = now + NET_BACK_DELAY;
                            if soon < at {
                                rb.stage = Stage::Waiting { at: soon };
                            }
                            rb.expedited = true;
                            out.push(log_action("rebuild expedited reason=network-back".to_string()));
                        }
                    }
                }
            }
            return out;
        }
        let (strikes, first) = {
            let Some(cur) = self.cur.as_mut() else {
                return out;
            };
            if reason == DeadReason::StillLoading {
                // Still loading counts only while it makes no progress.
                let progressed = matches!(
                    (cur.strike_resources, report.resources),
                    (Some(before), Some(after)) if after > before
                );
                if progressed {
                    cur.reset_strikes();
                    cur.next_probe_at = now + LOADING_RETRY;
                    return out;
                }
                cur.strike_resources = report.resources;
            }
            cur.strikes += 1;
            if cur.first_strike_at.is_none() {
                cur.first_strike_at = Some(now);
            }
            cur.next_probe_at = now + CONFIRM_GAP;
            (cur.strikes, cur.first_strike_at.unwrap_or(now))
        };
        if strikes >= 2 && now.saturating_duration_since(first) >= CONFIRM_GAP {
            out.extend(self.verdict_dead(now, Trigger::Dead(reason), call_active));
        }
        out
    }

    /// A probe failed or got no answer.
    fn no_answer(&mut self, now: Instant, call_active: bool, why: &str, id: u64) -> Vec<Action> {
        let mut out = Vec::new();
        let (gen, since, silent, strikes) = {
            let Some(cur) = self.cur.as_mut() else {
                return out;
            };
            if cur.no_answer_since.is_none() {
                cur.no_answer_since = Some(now);
            }
            cur.next_probe_at = now + NO_ANSWER_RETRY;
            let since_nav = now.saturating_duration_since(cur.nav_started);
            let newest = cur.newest_real;
            // Silence that is still a definite death: the newest navigation
            // committed a failure and nothing newer started, or the web view
            // never committed any document and its load is long overdue
            // (WebView2 holds a script back until a navigation commits, so a
            // hung or cut-off load can only ever time out).
            let silent = if since_nav < NAV_GRACE {
                None
            } else if let Some(f) = cur
                .failed_commit
                .filter(|f| newest.map_or(true, |n| n == f.id))
            {
                Some(DeadReason::Silent(f.class))
            } else if !cur.committed_any
                && cur.nav_in_flight.is_some()
                && since_nav >= IN_FLIGHT_LIMIT
            {
                Some(DeadReason::NeverLoaded)
            } else {
                None
            };
            (
                cur.gen,
                cur.no_answer_since.unwrap_or(now),
                silent,
                cur.strikes,
            )
        };
        let word = format!("no-answer:{why}");
        let line = format!("probe gen={gen} id={id} verdict={word} strikes={strikes}");
        if let Some(a) = self.probe_line(now, &word, line) {
            out.push(a);
        }
        if self.rebuild.is_some() {
            return out;
        }
        if let Some(reason) = silent {
            let (strikes, first) = {
                let Some(cur) = self.cur.as_mut() else {
                    return out;
                };
                cur.strikes += 1;
                if cur.first_strike_at.is_none() {
                    cur.first_strike_at = Some(now);
                }
                cur.next_probe_at = now + CONFIRM_GAP;
                (cur.strikes, cur.first_strike_at.unwrap_or(now))
            };
            if strikes >= 2 && now.saturating_duration_since(first) >= CONFIRM_GAP {
                out.extend(self.verdict_dead(now, Trigger::Dead(reason), call_active));
            }
            return out;
        }
        if call_active {
            // A page that does not answer may still be carrying a call (the
            // softphone frame runs in its own process): the guard stays.
            let first_time = match self.cur.as_mut() {
                Some(c) => {
                    let first = !c.hold_logged;
                    c.hold_logged = true;
                    first
                }
                None => false,
            };
            if first_time {
                out.push(log_action(format!("no answer during a call: waiting gen={gen}")));
            }
            return out;
        }
        if now.saturating_duration_since(since) >= UNRESPONSIVE_LIMIT {
            out.extend(self.verdict_dead(now, Trigger::Unresponsive, call_active));
        }
        out
    }

    // ── Verdicts, rebuilds, the restart ────────────────────────────

    fn verdict_dead(&mut self, now: Instant, trigger: Trigger, call_active: bool) -> Vec<Action> {
        let mut out = Vec::new();
        // A flagged call keeps the page unless its death is unambiguous.
        if call_active && !unambiguous(trigger) {
            if let Some(cur) = self.cur.as_mut() {
                cur.reset_strikes();
                cur.next_probe_at = now + WAIT_PROBE;
                if !cur.hold_logged {
                    cur.hold_logged = true;
                    out.push(log_action(format!(
                        "held gen={} reason=ambiguous-no-app trigger={} call-flag=1",
                        cur.gen,
                        trigger.word()
                    )));
                }
            }
            return out;
        }
        if self.dead_since.is_none() {
            self.dead_since = Some(now);
        }
        let gen = self
            .cur
            .as_ref()
            .map_or_else(|| "-".to_string(), |c| c.gen.to_string());
        out.push(log_action(format!(
            "dead gen={gen} reason={} call-flag={}",
            trigger.word(),
            call_active as u8
        )));
        if trigger.definite() {
            // No live page means no live call in it (rule 2c).
            out.push(Action::ClearStaleCall);
        }
        if let Some(cur) = self.cur.as_mut() {
            cur.reset_strikes();
            cur.no_answer_since = None;
            cur.next_probe_at = now + WAIT_PROBE;
        }
        if self.signout_pending {
            // Finish the sign-out a stale call flag held up; never rebuild.
            out.push(Action::FinishSignOut);
            return out;
        }
        if trigger.arms_restart() {
            out.extend(self.arm_restart(now, trigger));
        }
        if self.engine_gone {
            out.push(log_action("rebuild skipped reason=browser-process-gone".to_string()));
            return out;
        }
        out.extend(self.schedule_rebuild(now, trigger));
        out
    }

    fn schedule_rebuild(&mut self, now: Instant, trigger: Trigger) -> Vec<Action> {
        let mut out = Vec::new();
        if self.rebuild.is_some() {
            return out;
        }
        let mut delay = backoff_delay(self.attempts);
        if let Some(t) = self.net_back_at.take() {
            if now.saturating_duration_since(t) <= NET_BACK_WINDOW && delay > NET_BACK_DELAY {
                delay = NET_BACK_DELAY;
            }
        }
        let id = self.next_seq();
        self.rebuild = Some(Rebuild {
            id,
            trigger,
            stage: Stage::Waiting { at: now + delay },
            expedited: false,
        });
        out.push(log_action(format!(
            "rebuild #{} scheduled in={}s trigger={} id={id}",
            self.attempts + 1,
            delay.as_secs(),
            trigger.word()
        )));
        if self.attempts >= STUCK_AFTER && !self.stuck_logged {
            self.stuck_logged = true;
            out.push(log_action(format!(
                "engine may be stuck: {} rebuilds have not brought the Phone page back (server down, or a stuck WebView2 engine that only a restart replaces)",
                self.attempts
            )));
        }
        out
    }

    fn arm_restart(&mut self, now: Instant, trigger: Trigger) -> Vec<Action> {
        let mut out = Vec::new();
        if self.restart.is_some() {
            return out;
        }
        let (watched, last_class) = match &self.cur {
            Some(c) => (c.watched, c.last_class),
            None => (false, None),
        };
        if !watched {
            return out;
        }
        let evidence = restart_evidence(trigger, last_class, self.last_online);
        let min_pending = if matches!(trigger, Trigger::RendererGone(_)) {
            RESTART_MIN_PENDING_RENDERER
        } else {
            Duration::ZERO
        };
        let immediate = evidence == Evidence::Engine && min_pending == Duration::ZERO;
        self.restart = Some(RestartPending {
            armed_at: now,
            trigger,
            evidence,
            server_ok: false,
            min_pending,
            last_check: if immediate { Some(now) } else { None },
            last_hold: None,
        });
        out.push(log_action(format!(
            "restart pending reason={} evidence={}",
            trigger.word(),
            evidence.word()
        )));
        if immediate {
            // Safe right now? Then restart instead of building a second
            // softphone that is expected to stay stuck.
            out.push(Action::TryRestart);
        }
        out
    }

    fn restart_tick(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let online = self.last_online;
        let Some(r) = self.restart.as_mut() else {
            return out;
        };
        if now.saturating_duration_since(r.armed_at) < r.min_pending {
            return out;
        }
        if r
            .last_check
            .is_some_and(|t| now.saturating_duration_since(t) < RESTART_CHECK)
        {
            return out;
        }
        r.last_check = Some(now);
        let hold = if online == Some(false) {
            Some("network")
        } else if r.evidence != Evidence::Engine && !r.server_ok {
            Some(r.evidence.word())
        } else {
            None
        };
        match hold {
            Some(word) => {
                if let Some(line) = note_hold(r, now, word, "") {
                    out.push(log_action(line));
                }
            }
            None => out.push(Action::TryRestart),
        }
        out
    }

    /// The shell held a restart (a call, someone at the computer, the
    /// cooldown, ...): the line to log, at most once per reason per 10 min.
    pub fn restart_held(&mut self, now: Instant, word: &str, detail: &str) -> Option<String> {
        let r = self.restart.as_mut()?;
        note_hold(r, now, word, detail)
    }

    /// A ready softphone cancels the pending restart, if the signal came after
    /// the restart was armed.
    fn restart_ready(&mut self, signal: &str, signal_at: Instant) -> Vec<Action> {
        let armed_at = match &self.restart {
            Some(r) => r.armed_at,
            None => return Vec::new(),
        };
        if signal_at < armed_at {
            return Vec::new();
        }
        self.restart = None;
        vec![log_action(format!("restart canceled reason=ready signal={signal}"))]
    }

    fn go_idle(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(rb) = self.rebuild.take() {
            out.push(log_action(format!("rebuild canceled reason=signed-out id={}", rb.id)));
        }
        if self.restart.take().is_some() {
            out.push(log_action("restart canceled reason=signed-out".to_string()));
        }
        self.signout_pending = false;
        self.attempts = 0;
        self.missing_since = None;
        self.dead_since = None;
        self.wanted = None;
        self.stuck_logged = false;
        if !self.idle_logged {
            self.idle_logged = true;
            out.push(log_action("stop reason=no-owner".to_string()));
        }
        out
    }

    // ── The tick ───────────────────────────────────────────────────

    pub fn on_tick(&mut self, now: Instant, s: &Snapshot) -> Vec<Action> {
        let mut out = Vec::new();
        if s.exiting {
            return out;
        }
        if self.wanted.is_none() && s.owner.is_none() {
            out.extend(self.go_idle());
            return out;
        }
        self.idle_logged = false;
        if let Some(cur) = self.cur.as_mut() {
            cur.watched = s.phone_pinned && s.phone.as_deref() == Some(cur.label.as_str());
        }
        out.extend(self.restart_tick(now));

        // Rebuild progress.
        let pending = self.rebuild.as_ref().map(|rb| (rb.id, rb.trigger, rb.stage));
        if let Some((id, trigger, stage)) = pending {
            match stage {
                Stage::Waiting { at } if now >= at => {
                    if trigger == Trigger::Unresponsive && s.call_active {
                        // A call started after all.
                        self.rebuild = None;
                        out.push(log_action(format!("rebuild canceled reason=call id={id}")));
                    } else {
                        if let Some(rb) = self.rebuild.as_mut() {
                            rb.stage = Stage::Closing { since: now };
                        }
                        if trigger != Trigger::PageAsked {
                            self.attempts += 1;
                        }
                        out.push(Action::RunRebuild { id, trigger });
                        return out;
                    }
                }
                Stage::Waiting { .. } => {}
                Stage::Closing { since } => {
                    if now.saturating_duration_since(since) >= OPEN_LOST {
                        // open_tab_ex failed or deduped; "missing" takes over.
                        self.rebuild = None;
                        out.push(log_action(format!("rebuild lost id={id}")));
                    } else {
                        return out;
                    }
                }
            }
        }

        // An owner but no Phone tab at all.
        let phone = match (&s.phone, s.phone_has_webview) {
            (Some(label), true) => label.clone(),
            _ => {
                if self.signout_pending || self.rebuild.is_some() || self.engine_gone {
                    return out;
                }
                let since = *self.missing_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= MISSING_CONFIRM {
                    self.missing_since = None;
                    out.extend(self.verdict_dead(now, Trigger::Missing, s.call_active));
                }
                return out;
            }
        };
        self.missing_since = None;

        // Only the pinned, always-on tab is watched (nobody without phone
        // access ever gets one)...
        if !s.phone_pinned {
            if !self.unpinned_logged {
                self.unpinned_logged = true;
                out.push(log_action("unpinned phone tab: not watched".to_string()));
            }
            return out;
        }
        self.unpinned_logged = false;
        // ...and only once its web view is the one being followed.
        if !self.cur.as_ref().is_some_and(|c| c.label == phone) {
            return out;
        }

        // A probe that never came back.
        let timed_out = self
            .cur
            .as_ref()
            .and_then(|c| c.probe)
            .filter(|p| now.saturating_duration_since(p.1) >= PROBE_TIMEOUT)
            .map(|p| p.0);
        if let Some(id) = timed_out {
            if let Some(cur) = self.cur.as_mut() {
                cur.probe = None;
                cur.late_probe = Some(id);
            }
            out.extend(self.no_answer(now, s.call_active, "timeout", id));
        }

        // Probe when due (never inside the grace, unless navigations storm).
        let due = self.cur.as_ref().is_some_and(|c| {
            c.probe.is_none()
                && now >= c.next_probe_at
                && (now.saturating_duration_since(c.nav_started) >= NAV_GRACE || c.storm())
        });
        if due {
            let id = self.next_seq();
            if let Some(cur) = self.cur.as_mut() {
                cur.probe = Some((id, now));
                out.push(Action::Probe {
                    label: cur.label.clone(),
                    gen: cur.gen,
                    id,
                });
            }
        }
        out
    }

    // ── Hooks from the shell ───────────────────────────────────────

    /// Sign-out (close_phone_tabs): stop, even if a live call keeps the tab.
    pub fn on_signed_out(&mut self, _now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        self.signout_pending = true;
        self.wanted = None;
        if let Some(rb) = self.rebuild.take() {
            out.push(log_action(format!("rebuild canceled reason=signed-out id={}", rb.id)));
        }
        if self.restart.take().is_some() {
            out.push(log_action("restart canceled reason=signed-out".to_string()));
        }
        out.push(log_action("signed out: watchdog stopped".to_string()));
        out
    }

    /// ensure_phone_tab: whether it should go ahead (false while our rebuild of
    /// this person's tab is already on its way).
    pub fn on_ensure(&mut self, now: Instant, owner: &str, current_owner: Option<&str>) -> (bool, Vec<Action>) {
        let mut out = Vec::new();
        self.signout_pending = false;
        let prev_wanted = self.wanted.replace(owner.to_string());
        let same = current_owner.is_none_or(|o| o == owner) || prev_wanted.as_deref() == Some(owner);
        let pending = self
            .rebuild
            .as_ref()
            .map(|rb| (matches!(rb.stage, Stage::Waiting { .. }), rb.expedited));
        if let Some((waiting, expedited)) = pending {
            if same {
                // The Platform tab just loaded, so the server answers: move a
                // long wait to now, once.
                if waiting && !expedited {
                    if let Some(rb) = self.rebuild.as_mut() {
                        rb.stage = Stage::Waiting { at: now };
                        rb.expedited = true;
                    }
                    out.push(log_action("ensure expedite".to_string()));
                }
                out.push(log_action("ensure skip (rebuild pending)".to_string()));
                return (false, out);
            }
            // Someone else's phone now: ensure replaces the tab, as before.
            self.rebuild = None;
            self.attempts = 0;
            out.push(log_action("ensure owner=changed: rebuild canceled".to_string()));
            return (true, out);
        }
        // Check the kept tab now: how a Platform reload heals a dead Phone tab
        // that ensure keeps because its address is still the voice page.
        if let Some(cur) = self.cur.as_mut() {
            if cur.probe.is_none() && now.saturating_duration_since(cur.nav_started) >= NAV_GRACE {
                cur.probe_soon(now);
            }
        }
        out.push(log_action(format!(
            "ensure proceed owner={}",
            if same { "same" } else { "changed" }
        )));
        (true, out)
    }

    /// The page's own reconnect sentinel: a ticket for an immediate rebuild,
    /// or None (one is already pending, or the person signed out).
    pub fn on_page_reconnect(&mut self, now: Instant) -> (Option<u64>, Vec<Action>) {
        let mut out = Vec::new();
        if self.signout_pending {
            out.push(log_action("page reconnect after sign-out: finishing the sign-out".to_string()));
            out.push(Action::FinishSignOut);
            return (None, out);
        }
        if self.rebuild.is_some() {
            out.push(log_action("page reconnect ignored: rebuild pending".to_string()));
            return (None, out);
        }
        let id = self.next_seq();
        self.rebuild = Some(Rebuild {
            id,
            trigger: Trigger::PageAsked,
            stage: Stage::Closing { since: now },
            expedited: true,
        });
        out.push(log_action(format!("page reconnect id={id}")));
        (Some(id), out)
    }

    /// Any bridge sentinel from `label`: its page's JavaScript is running.
    pub fn on_page_alive(&mut self, _now: Instant, label: &str) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(cur) = self.cur.as_mut() else {
            return out;
        };
        if cur.label != label {
            return out;
        }
        cur.reset_strikes();
        cur.no_answer_since = None;
        let cancel = match &self.rebuild {
            Some(rb) if matches!(rb.stage, Stage::Waiting { .. }) && rb.trigger != Trigger::PageAsked => {
                Some(rb.id)
            }
            _ => None,
        };
        if let Some(id) = cancel {
            self.rebuild = None;
            out.push(log_action(format!("page alive: rebuild canceled id={id}")));
        }
        out
    }

    /// PHONE_STATE from `label` (available | call | offline | connecting).
    /// `available` and `call` mean the softphone works: a pending restart is
    /// canceled. `offline` is either a break (the softphone works) or a page
    /// that gave up (voice page halt(): stuck), so it only asks for a probe,
    /// whose `a` field (the agent arrived) tells them apart.
    pub fn on_phone_state(&mut self, now: Instant, label: &str, state: &str) -> Vec<Action> {
        let mut out = Vec::new();
        if !self.cur.as_ref().is_some_and(|c| c.label == label) {
            return out;
        }
        match state {
            "available" | "call" => out.extend(self.restart_ready(state, now)),
            "offline" if self.restart.is_some() => {
                if let Some(cur) = self.cur.as_mut() {
                    if cur.probe.is_none() && cur.next_probe_at > now {
                        cur.next_probe_at = now;
                    }
                }
                out.push(log_action(
                    "phone state offline: checking whether the softphone is up".to_string(),
                ));
            }
            _ => {}
        }
        out
    }

    /// The queued open step of rebuild `id` may still run.
    pub fn open_step_allowed(&self, id: u64) -> bool {
        matches!(&self.rebuild, Some(rb) if rb.id == id && matches!(rb.stage, Stage::Closing { .. }))
    }

    pub fn cancel_rebuild(&mut self, id: u64, why: &str) -> Vec<Action> {
        if self.rebuild.as_ref().is_some_and(|rb| rb.id == id) {
            self.rebuild = None;
            vec![log_action(format!("rebuild canceled reason={why} id={id}"))]
        } else {
            Vec::new()
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const COMPANY: &str = "3fa85f64-5717-4562-b3fc-2c963f66afa6";
    const OWNER: &str = "3fa85f64-5717-4562-b3fc-2c963f66afa6/9b2e8c1a-0d4f-4e6a-8b7c-1f2e3d4c5b6a";
    const OTHER: &str = "11111111-2222-3333-4444-555555555555/66666666-7777-8888-9999-aaaaaaaaaaaa";

    fn secs(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    fn page(k: &str, next: bool, ready: &str, status: Option<u16>) -> serde_json::Value {
        serde_json::json!({
            "k": k, "n": next, "c": false, "q": false, "a": false, "r": ready,
            "s": status, "t": 5, "o": true, "p": format!("/manage/{COMPANY}/voice"),
        })
    }

    fn set(mut v: serde_json::Value, key: &str, val: serde_json::Value) -> String {
        v[key] = val;
        v.to_string()
    }

    fn alive() -> String {
        page("p", true, "complete", Some(200)).to_string()
    }

    fn alb502() -> String {
        page("p", false, "complete", Some(502)).to_string()
    }

    fn blank() -> String {
        page("b", false, "complete", None).to_string()
    }

    fn error_page() -> String {
        page("e", false, "complete", None).to_string()
    }

    fn snap(owner: Option<&str>, phone: Option<&str>, pinned: bool, call: bool) -> Snapshot {
        Snapshot {
            exiting: false,
            owner: owner.map(str::to_string),
            phone: phone.map(str::to_string),
            phone_pinned: pinned,
            phone_has_webview: phone.is_some(),
            call_active: call,
        }
    }

    fn nav_start(id: u64, at: Instant) -> NavStart {
        NavStart {
            id,
            at,
            sentinel: false,
            target: "platform:/manage/:id/voice".to_string(),
            redirected: false,
        }
    }

    fn nav_done(id: u64, at: Instant, ok: bool, err: i32, http: Option<i32>) -> NavDone {
        NavDone {
            id,
            at,
            ok,
            err,
            http,
            source: Some(PageKind::Platform),
        }
    }

    /// The pinned Phone web view "tab-1" (gen 1), created at t0.
    fn watched(t0: Instant) -> Watch {
        let mut w = Watch::new();
        let _ = w.on_webview_created(t0, "tab-1", 1, true);
        w
    }

    /// The same, with its first load arrived (navigation 1, OK at 1 s).
    fn loaded(t0: Instant) -> Watch {
        let mut w = watched(t0);
        let _ = w.on_nav_starting(1, nav_start(1, t0));
        let _ = w.on_nav_completed(secs(t0, 1), 1, nav_done(1, secs(t0, 1), true, 0, Some(200)));
        w
    }

    fn inject_probe(w: &mut Watch, now: Instant) -> (u64, u64) {
        w.seq += 1;
        let id = w.seq;
        let cur = w.cur.as_mut().expect("a Phone web view");
        cur.probe = Some((id, now));
        (cur.gen, id)
    }

    /// Answer a probe at `now` (one is put in flight first).
    fn answer(w: &mut Watch, now: Instant, json: &str, call: bool) -> Vec<Action> {
        let (gen, id) = inject_probe(w, now);
        w.on_probe_answer(now, gen, id, ProbeOutcome::Json(json.to_string()), call)
    }

    fn logs(a: &[Action]) -> Vec<String> {
        a.iter()
            .filter_map(|x| match x {
                Action::Log(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    fn has_log(a: &[Action], needle: &str) -> bool {
        logs(a).iter().any(|m| m.contains(needle))
    }

    fn rebuilds(a: &[Action]) -> usize {
        a.iter()
            .filter(|x| matches!(x, Action::RunRebuild { .. }))
            .count()
    }

    fn clears(a: &[Action]) -> bool {
        a.iter().any(|x| matches!(x, Action::ClearStaleCall))
    }

    fn tries_restart(a: &[Action]) -> bool {
        a.iter().any(|x| matches!(x, Action::TryRestart))
    }

    fn probes(a: &[Action]) -> Vec<(u64, u64)> {
        a.iter()
            .filter_map(|x| match x {
                Action::Probe { gen, id, .. } => Some((*gen, *id)),
                _ => None,
            })
            .collect()
    }

    fn test_rebuild(id: u64, trigger: Trigger, stage: Stage) -> Rebuild {
        Rebuild {
            id,
            trigger,
            stage,
            expedited: false,
        }
    }

    /// Drives the machine tick by tick; a probe is answered at once with
    /// `page` (None: never answered).
    struct Sim {
        w: Watch,
        t0: Instant,
        owner: Option<String>,
        label: String,
        pinned: bool,
        has_tab: bool,
        call: bool,
        page: Option<String>,
    }

    impl Sim {
        fn new(t0: Instant, w: Watch) -> Sim {
            Sim {
                w,
                t0,
                owner: Some(OWNER.to_string()),
                label: "tab-1".to_string(),
                pinned: true,
                has_tab: true,
                call: false,
                page: Some(alive()),
            }
        }

        fn snapshot(&self) -> Snapshot {
            Snapshot {
                exiting: false,
                owner: self.owner.clone(),
                phone: if self.has_tab { Some(self.label.clone()) } else { None },
                phone_pinned: self.pinned,
                phone_has_webview: self.has_tab,
                call_active: self.call,
            }
        }

        fn tick(&mut self, s: u64) -> Vec<Action> {
            let now = secs(self.t0, s);
            let snapshot = self.snapshot();
            let mut out = self.w.on_tick(now, &snapshot);
            for (gen, id) in probes(&out) {
                if let Some(json) = self.page.clone() {
                    let more = self
                        .w
                        .on_probe_answer(now, gen, id, ProbeOutcome::Json(json), self.call);
                    out.extend(more);
                }
            }
            out
        }

        fn run(&mut self, from: u64, to: u64) -> Vec<Action> {
            let mut all = Vec::new();
            let mut s = from;
            while s <= to {
                all.extend(self.tick(s));
                s += 5;
            }
            all
        }
    }

    // ── Pure functions ─────────────────────────────────────────────

    #[test]
    fn path_template_vectors() {
        let uuid = "3fa85f64-5717-4562-b3fc-2c963f66afa6";
        assert_eq!(path_template(&format!("/manage/{uuid}/voice")), "/manage/:id/voice");
        assert_eq!(
            path_template("/manage/3FA85F64-5717-4562-B3FC-2C963F66AFA6/voice/"),
            "/manage/:id/voice"
        );
        assert_eq!(
            path_template(&format!("/manage/{uuid}/clients/{uuid}")),
            "/manage/:id/clients/:id"
        );
        assert_eq!(path_template(&format!("/manage/{uuid}/login")), "/manage/:id/login");
        assert_eq!(path_template("/orders/12345"), "/orders/:n");
        assert_eq!(path_template("/page2"), "/page:n");
        assert_eq!(path_template("/a/jane.doe@example.com"), "/a/:x");
        assert_eq!(path_template("/t/abcdef0123456789abcd"), "/t/:x");
        assert_eq!(path_template("/x/%E2%9C%93"), "/x/:x");
        assert_eq!(path_template(""), "/");
        assert_eq!(path_template("/"), "/");
        assert_eq!(path_template("/voice?companyId=abc"), "/voice");
        assert_eq!(path_template("/a/b/c/d/e/f/g/h/i/j"), "/a/b/c/d/e/f/g/h/...");
        assert_eq!(path_template("/voice-settings"), "/voice-settings");
        assert_eq!(path_template("/voice/signin-complete"), "/voice/signin-complete");
    }

    #[test]
    fn url_kind_vectors() {
        assert_eq!(url_kind("about:blank"), PageKind::Blank);
        assert_eq!(url_kind(""), PageKind::Blank);
        assert_eq!(url_kind("https://platform.vistainterface.com/x"), PageKind::Platform);
        assert_eq!(url_kind("https://PLATFORM.vistainterface.com/x"), PageKind::Platform);
        assert_eq!(url_kind("https://app.vistainterface.com/login"), PageKind::Vista);
        assert_eq!(url_kind("https://vistainterface.com/"), PageKind::Vista);
        assert_eq!(url_kind("https://evilvistainterface.com/"), PageKind::Other);
        assert_eq!(url_kind("https://platform.vistainterface.com.evil.com/"), PageKind::Other);
        assert_eq!(url_kind("https://accounts.google.com/"), PageKind::Other);
        assert_eq!(url_kind("chrome-error://chromewebdata/"), PageKind::Error);
        assert_eq!(url_kind("http://platform.vistainterface.com/"), PageKind::Error);
        assert_eq!(url_kind("not a url"), PageKind::Error);
    }

    #[test]
    fn sentinels_recognised() {
        for s in [
            "https://vista-desktop.invalid/__close__",
            "https://vista-desktop.invalid/__call_active__/1",
            "https://vista-desktop.invalid/__agent_ready__",
            "https://vista-desktop.invalid/__reconnect_phone__",
            "https://vista-desktop.invalid/__phone_always_on__/a/b",
            "https://vista-desktop.invalid/__phone_signed_out__",
            "https://vista-desktop.invalid/__phone_state__/available",
            "https://vista-desktop.invalid/__call_ringing__/x",
            "https://vista-desktop.invalid/__call_ring_over__/x",
            "https://vista-desktop.invalid/__restart_app__/t",
        ] {
            assert!(is_sentinel(s), "{s}");
        }
        assert!(!is_sentinel("https://platform.vistainterface.com/"));
    }

    #[test]
    fn log_target_never_leaks() {
        let url = format!(
            "https://platform.vistainterface.com/manage/{COMPANY}/clients/9b2e8c1a-0d4f-4e6a-8b7c-1f2e3d4c5b6a?phone=5715550142&name=Jane"
        );
        let t = log_target(&url);
        assert_eq!(t, "platform:/manage/:id/clients/:id");
        assert!(!t.contains('?') && !t.contains('=') && !t.contains("Jane"));
        let chars: Vec<char> = t.chars().collect();
        assert!(!chars.windows(4).any(|w| w.iter().all(|c| c.is_ascii_digit())));
        assert_eq!(log_target("https://accounts.google.com/o/oauth2/auth?client_id=123"), "other");
        assert_eq!(log_target("about:blank"), "blank");
        assert_eq!(log_target("chrome-error://chromewebdata/"), "error");
        assert_eq!(log_target("https://app.vistainterface.com/login?next=%2Fmanage"), "vista:/login");
        assert_eq!(log_target("https://platform.vistainterface.com"), "platform:/");
    }

    #[test]
    fn classify_nav_table() {
        let plat = Some(PageKind::Platform);
        assert_eq!(classify_nav(true, 0, Some(200), plat), NavClass::Ok);
        assert_eq!(classify_nav(false, 0, Some(502), plat), NavClass::Server(502));
        assert_eq!(classify_nav(true, 0, Some(503), None), NavClass::Server(503));
        assert_eq!(classify_nav(false, 12, Some(0), None), NavClass::Network);
        assert_eq!(classify_nav(false, 11, None, None), NavClass::Network);
        assert_eq!(classify_nav(false, 13, Some(0), None), NavClass::Network);
        assert_eq!(classify_nav(false, 12, Some(200), None), NavClass::Other);
        assert_eq!(classify_nav(false, 14, Some(0), plat), NavClass::Canceled);
        assert_eq!(classify_nav(false, 9, Some(0), None), NavClass::Canceled);
        assert_eq!(classify_nav(false, 9, Some(200), None), NavClass::Canceled);
        assert_eq!(
            classify_nav(false, 14, Some(0), Some(PageKind::Blank)),
            NavClass::BlankAfterFailure
        );
        assert_eq!(classify_nav(false, 2, Some(0), None), NavClass::Other);
        assert_eq!(classify_nav(false, 0, Some(404), None), NavClass::Other);
        for c in [NavClass::Server(500), NavClass::Network, NavClass::BlankAfterFailure] {
            assert!(c.strong(), "{c:?}");
        }
        for c in [NavClass::Ok, NavClass::Canceled, NavClass::Other] {
            assert!(!c.strong(), "{c:?}");
        }
    }

    #[test]
    fn web_error_and_process_names() {
        assert_eq!(web_error_name(0), "UNKNOWN");
        assert_eq!(web_error_name(9), "CONNECTION_ABORTED");
        assert_eq!(web_error_name(11), "DISCONNECTED");
        assert_eq!(web_error_name(14), "OPERATION_CANCELED");
        assert_eq!(web_error_name(18), "VALID_PROXY_AUTHENTICATION_REQUIRED");
        assert_eq!(web_error_name(99), "OTHER");
        assert_eq!(process_kind_name(0), "BROWSER_PROCESS_EXITED");
        assert_eq!(process_kind_name(1), "RENDER_PROCESS_EXITED");
        assert_eq!(process_kind_name(42), "OTHER");
    }

    #[test]
    fn parse_probe_cases() {
        let full = parse_probe(&alive()).expect("parses");
        assert_eq!(full.kind, PageKind::Platform);
        assert!(full.next && !full.hook && !full.streams && !full.agent);
        assert_eq!(full.ready, Ready::Complete);
        assert_eq!(full.status, Some(200));
        assert_eq!(full.resources, Some(5));
        assert_eq!(full.online, Some(true));
        assert_eq!(full.path_tpl, "/manage/:id/voice");
        let bare = parse_probe("{\"k\":\"b\"}").expect("defaults");
        assert!(!bare.next && !bare.agent);
        assert_eq!(bare.status, None);
        assert_eq!(bare.online, None);
        assert_eq!(bare.resources, None);
        assert_eq!(bare.ready, Ready::Unknown);
        assert_eq!(bare.path_tpl, "");
        assert!(parse_probe("{\"k\":\"x\"}").is_none());
        assert!(parse_probe("null").is_none());
        assert!(parse_probe("garbage").is_none());
        assert_eq!(parse_probe("{\"k\":\"p\",\"s\":0}").map(|r| r.status), Some(None));
        assert_eq!(
            parse_probe("{\"k\":\"p\",\"r\":\"weird\"}").map(|r| r.ready),
            Some(Ready::Unknown)
        );
    }

    #[test]
    fn probe_script_is_safe() {
        assert!(!PROBE_JS.contains('\\'));
        assert!(!PROBE_JS.contains('"'));
        for field in ["k:k", "n:n", "c:c", "q:q", "a:a", "s:s", "t:t", "o:o"] {
            assert!(PROBE_JS.contains(field), "{field}");
        }
        assert!(PROBE_JS.contains("__VISTA_ANSWER_CALL"));
        assert!(PROBE_JS.contains("agent.initialized===true"));
        assert!(!PROBE_JS.contains("_next/static"), "a chunk tag is no proof of life");
    }

    #[test]
    fn judge_probe_table() {
        let d = |s: u64| Duration::from_secs(s);
        let r = |j: String| parse_probe(&j).expect("parses");
        let next500 = page("p", true, "complete", Some(500)).to_string();
        assert_eq!(judge_probe(&r(alive()), d(25), None, false), Verdict::Alive);
        assert_eq!(
            judge_probe(&r(next500.clone()), d(25), None, false),
            Verdict::Dead(DeadReason::ServerError(500))
        );
        assert_eq!(
            judge_probe(&r(page("v", true, "complete", Some(200)).to_string()), d(25), None, false),
            Verdict::Alive
        );
        assert_eq!(
            judge_probe(&r(page("o", false, "complete", None).to_string()), d(25), None, false),
            Verdict::Alive
        );
        assert_eq!(judge_probe(&r(blank()), d(25), None, false), Verdict::Dead(DeadReason::Blank));
        assert_eq!(judge_probe(&r(blank()), d(30), None, true), Verdict::Loading);
        assert_eq!(judge_probe(&r(blank()), d(80), None, true), Verdict::Dead(DeadReason::Blank));
        assert_eq!(
            judge_probe(&r(error_page()), d(25), None, false),
            Verdict::Dead(DeadReason::ErrorPage)
        );
        assert_eq!(
            judge_probe(&r(alb502()), d(25), None, false),
            Verdict::Dead(DeadReason::NoApp(Some(502)))
        );
        let interactive = page("p", false, "interactive", None).to_string();
        assert_eq!(judge_probe(&r(interactive.clone()), d(30), None, false), Verdict::Loading);
        assert_eq!(
            judge_probe(&r(interactive.clone()), d(50), None, false),
            Verdict::Dead(DeadReason::StillLoading)
        );
        assert_eq!(
            judge_probe(&r(interactive), d(60), Some(d(30)), false),
            Verdict::Loading,
            "the load clock starts at ContentLoading"
        );
        assert_eq!(judge_probe(&r(alb502()), d(15), None, false), Verdict::Loading);
        assert_eq!(judge_probe(&r(next500), d(30), None, true), Verdict::Loading);
    }

    #[test]
    fn any_app_marker_means_alive() {
        let d = Duration::from_secs(60);
        let mut r = parse_probe(&alb502()).expect("parses");
        assert_eq!(judge_probe(&r, d, None, false), Verdict::Dead(DeadReason::NoApp(Some(502))));
        r.hook = true;
        assert_eq!(judge_probe(&r, d, None, false), Verdict::Alive, "the voice page's hook");
        r.hook = false;
        r.streams = true;
        assert_eq!(judge_probe(&r, d, None, false), Verdict::Alive, "Streams is loaded");
        r.streams = false;
        r.status = Some(200);
        r.next = true;
        assert_eq!(judge_probe(&r, d, None, false), Verdict::Alive, "Next on any path");
    }

    #[test]
    fn unambiguous_and_evidence_table() {
        assert!(unambiguous(Trigger::Dead(DeadReason::Blank)));
        assert!(unambiguous(Trigger::Dead(DeadReason::ErrorPage)));
        assert!(unambiguous(Trigger::Dead(DeadReason::NoApp(Some(502)))));
        assert!(unambiguous(Trigger::Dead(DeadReason::NeverLoaded)));
        assert!(unambiguous(Trigger::Dead(DeadReason::Silent(NavClass::Network))));
        assert!(!unambiguous(Trigger::Dead(DeadReason::NoApp(Some(200)))));
        assert!(!unambiguous(Trigger::Dead(DeadReason::NoApp(None))));
        assert!(!unambiguous(Trigger::Dead(DeadReason::StillLoading)));
        assert!(unambiguous(Trigger::RendererGone(1)));
        assert!(!unambiguous(Trigger::Unresponsive));
        assert_eq!(
            restart_evidence(Trigger::Dead(DeadReason::Blank), Some(NavClass::Canceled), Some(true)),
            Evidence::Engine
        );
        assert_eq!(
            restart_evidence(Trigger::Dead(DeadReason::Blank), Some(NavClass::Network), Some(true)),
            Evidence::Network
        );
        assert_eq!(
            restart_evidence(Trigger::Dead(DeadReason::NoApp(Some(502))), None, None),
            Evidence::Server
        );
        assert_eq!(
            restart_evidence(Trigger::Dead(DeadReason::NoApp(Some(200))), None, None),
            Evidence::Unclear
        );
        assert_eq!(
            restart_evidence(Trigger::RendererGone(1), Some(NavClass::Ok), None),
            Evidence::Engine
        );
        assert_eq!(
            restart_evidence(Trigger::Dead(DeadReason::NeverLoaded), None, Some(false)),
            Evidence::Network
        );
        assert!(!Trigger::PageAsked.arms_restart());
        assert!(!Trigger::Unresponsive.arms_restart());
        assert!(!Trigger::Missing.arms_restart());
        assert!(Trigger::Dead(DeadReason::Blank).arms_restart());
        assert!(!Trigger::Unresponsive.definite() && !Trigger::PageAsked.definite());
    }

    #[test]
    fn backoff_schedule_and_stable_reset() {
        let delays: Vec<u64> = (0..7).map(|a| backoff_delay(a).as_secs()).collect();
        assert_eq!(delays, vec![10, 30, 60, 120, 300, 300, 300]);
        assert_eq!(backoff_delay(50).as_secs(), 300);
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.attempts = 3;
        let _ = w.on_alive(secs(t0, 100));
        let _ = w.on_alive(secs(t0, 220));
        assert_eq!(w.attempts, 3, "2 minutes alive does not reset the backoff");
        let a = w.on_alive(secs(t0, 400));
        assert_eq!(w.attempts, 0);
        assert!(has_log(&a, "backoff reset"));
    }

    #[test]
    fn voice_url_for_owner_cases() {
        assert_eq!(
            voice_url_for_owner(OWNER).as_deref(),
            Some("https://platform.vistainterface.com/manage/3fa85f64-5717-4562-b3fc-2c963f66afa6/voice")
        );
        assert_eq!(voice_url_for_owner("bad/x"), None);
        assert_eq!(voice_url_for_owner(""), None);
        assert_eq!(voice_url_for_owner(COMPANY), None);
    }

    #[test]
    fn utc_stamp_vectors() {
        assert_eq!(utc_stamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_stamp(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(utc_stamp(1_700_000_000_000), "2023-11-14T22:13:20.000Z");
        assert_eq!(utc_stamp(1_763_932_015_123), "2025-11-23T21:06:55.123Z");
        assert_eq!(utc_stamp(4_102_444_800_000), "2100-01-01T00:00:00.000Z");
    }

    #[test]
    fn idle_ms_handles_the_49_day_wrap() {
        assert_eq!(idle_ms(5, u32::MAX - 5), 11);
        assert_eq!(idle_ms(1_000, 400), 600);
        assert_eq!(idle_ms(400, 400), 0);
    }

    #[test]
    fn restart_history_rules() {
        let hour = 3_600_000;
        let now = 10 * RESTART_WINDOW_MS;
        assert_eq!(restart_decision(&[], now), Ok(vec![now]));
        assert_eq!(
            restart_decision(&[now - 600_000], now),
            Err(RestartHold::Cooldown { since_last_s: 600 })
        );
        let three = [now - 5 * hour, now - 4 * hour, now - 3 * hour];
        assert_eq!(restart_decision(&three, now), Err(RestartHold::Cap { count: 3 }));
        let old = [now - RESTART_WINDOW_MS, now - 7 * hour];
        assert_eq!(restart_decision(&old, now), Ok(vec![now]));
        assert_eq!(
            restart_decision(&[now + hour], now),
            Err(RestartHold::Cooldown { since_last_s: 0 }),
            "a time in the future (clock moved back) counts as now"
        );
        let two = [now - 2 * hour, now - 5 * hour];
        assert_eq!(restart_decision(&two, now), Ok(vec![now - 5 * hour, now - 2 * hour, now]));
        assert_eq!(normalize_history(&[now + hour], now), (vec![now], true));
        assert_eq!(normalize_history(&[now - hour], now), (vec![now - hour], false));
    }

    #[test]
    fn history_file_reading() {
        let now = 5_000_000;
        assert_eq!(history_from_read(&HistoryRead::Missing, now), (Vec::new(), false));
        assert_eq!(
            history_from_read(&HistoryRead::Text("[1,2]".to_string()), now),
            (vec![1, 2], false)
        );
        assert_eq!(
            history_from_read(&HistoryRead::Text("garbage".to_string()), now),
            (vec![now], true)
        );
        assert_eq!(
            history_from_read(&HistoryRead::Text("[-1]".to_string()), now),
            (vec![now], true)
        );
        assert_eq!(history_from_read(&HistoryRead::Unreadable, now), (vec![now], true));
        assert_eq!(parse_history(&history_json(&[3, 4])), Some(vec![3, 4]));
        assert_eq!(parse_history("{\"a\":1}"), None);
    }

    #[test]
    fn handoff_round_trip() {
        let now = 1_763_932_015_123;
        let text = handoff_json(OWNER, now);
        let (company, user) = parse_handoff(&text, now + 30_000).expect("a fresh hand-off");
        assert_eq!(format!("{company}/{user}"), OWNER);
        assert!(parse_handoff(&text, now + HANDOFF_MAX_AGE_MS + 1).is_none(), "too old");
        assert!(parse_handoff(&text, now - 60_000).is_none(), "from the future");
        assert!(parse_handoff(&handoff_json("bad/x", now), now).is_none());
        assert!(parse_handoff("garbage", now).is_none());
    }

    // ── The state machine ──────────────────────────────────────────

    #[test]
    fn healthy_page_never_rebuilds() {
        let t0 = Instant::now();
        let mut sim = Sim::new(t0, watched(t0));
        let all = sim.run(5, 1800);
        assert_eq!(rebuilds(&all), 0);
        assert!(!clears(&all));
        assert!(!tries_restart(&all));
        assert_eq!(probes(&all).len(), 30, "one probe a minute from 20 s");
    }

    #[test]
    fn no_probe_inside_grace() {
        let t0 = Instant::now();
        let mut w = watched(t0);
        let _ = w.on_nav_starting(1, nav_start(7, t0));
        let mut sim = Sim::new(t0, w);
        for s in [5, 10, 15] {
            assert!(probes(&sim.tick(s)).is_empty(), "no probe at {s} s");
        }
        assert_eq!(probes(&sim.tick(20)).len(), 1);
    }

    #[test]
    fn two_strikes_ten_seconds_apart_needed() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let a1 = answer(&mut w, secs(t0, 20), &alb502(), false);
        assert!(!clears(&a1) && !has_log(&a1, "scheduled"));
        let a2 = answer(&mut w, secs(t0, 25), &alb502(), false);
        assert!(!clears(&a2) && !has_log(&a2, "scheduled"), "5 s later is not enough");
        let a3 = answer(&mut w, secs(t0, 31), &alb502(), false);
        assert!(clears(&a3));
        assert!(has_log(&a3, "rebuild #1 scheduled in=10s"));
        assert_eq!(rebuilds(&a3), 0);
        let s = snap(Some(OWNER), Some("tab-1"), true, false);
        assert_eq!(rebuilds(&w.on_tick(secs(t0, 41), &s)), 1);
    }

    #[test]
    fn failed_navigation_only_probes() {
        let cases = [
            (false, 12, Some(0), Some(PageKind::Platform)),
            (false, 0, Some(502), Some(PageKind::Platform)),
            (false, 14, Some(0), Some(PageKind::Platform)),
            (false, 14, Some(0), Some(PageKind::Blank)),
            (false, 2, Some(0), None),
        ];
        for (ok, err, http, source) in cases {
            let t0 = Instant::now();
            let mut w = loaded(t0);
            let _ = answer(&mut w, secs(t0, 20), &alive(), false);
            let _ = w.on_nav_starting(1, nav_start(2, secs(t0, 30)));
            // In flight at 50 s: still loading, next look at 60 s.
            let _ = answer(&mut w, secs(t0, 50), &blank(), false);
            let done = NavDone {
                id: 2,
                at: secs(t0, 52),
                ok,
                err,
                http,
                source,
            };
            let a = w.on_nav_completed(secs(t0, 52), 1, done);
            assert_eq!(rebuilds(&a), 0, "{err}");
            assert!(!clears(&a), "{err}");
            let next = w.cur.as_ref().map(|c| c.next_probe_at);
            assert_eq!(next, Some(secs(t0, 52)), "probe at once ({err})");
        }
    }

    #[test]
    fn stale_generation_ignored() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = w.on_webview_created(secs(t0, 5), "tab-2", 2, true);
        assert!(w
            .on_nav_completed(secs(t0, 6), 1, nav_done(9, secs(t0, 6), false, 11, None))
            .is_empty());
        assert!(w.on_process_failed(secs(t0, 6), 1, 1, false).is_empty());
        assert!(w
            .on_probe_answer(secs(t0, 30), 1, 1, ProbeOutcome::Json(blank()), false)
            .is_empty());
        let (gen, id) = inject_probe(&mut w, secs(t0, 30));
        assert!(w
            .on_probe_answer(secs(t0, 31), gen, id + 100, ProbeOutcome::Json(blank()), false)
            .is_empty());
        assert!(w.rebuild.is_none());
    }

    #[test]
    fn closed_webview_ignored() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let a = w.on_webview_closed("tab-1");
        assert!(has_log(&a, "webview close gen=1"));
        assert!(w.on_process_failed(secs(t0, 5), 1, 1, false).is_empty());
        assert!(w.on_webview_closed("tab-1").is_empty());
    }

    #[test]
    fn unresponsive_with_call_never_rebuilds() {
        let t0 = Instant::now();
        let mut sim = Sim::new(t0, loaded(t0));
        sim.call = true;
        sim.page = None;
        let all = sim.run(5, 1800);
        assert_eq!(rebuilds(&all), 0);
        assert!(!clears(&all));
        assert!(!has_log(&all, "scheduled"));
        assert!(has_log(&all, "no answer during a call: waiting"));
    }

    #[test]
    fn unresponsive_without_call_rebuilds_after_two_minutes() {
        let t0 = Instant::now();
        let mut sim = Sim::new(t0, loaded(t0));
        sim.page = None;
        let early = sim.run(5, 155);
        assert!(!has_log(&early, "scheduled"), "no verdict before 2 min of silence");
        let verdict = sim.run(160, 185);
        assert!(has_log(&verdict, "dead gen=1 reason=unresponsive"));
        assert!(has_log(&verdict, "rebuild #1 scheduled in=10s"));
        assert!(!clears(&verdict), "an unanswered page may still carry a call");
        assert!(!tries_restart(&verdict));
        assert!(sim.w.restart.is_none(), "an unanswered page never arms the restart");
        // A call starts before the rebuild fires: it is canceled.
        sim.call = true;
        let fire = sim.run(190, 200);
        assert!(has_log(&fire, "rebuild canceled reason=call"));
        assert_eq!(rebuilds(&fire), 0);
    }

    #[test]
    fn renderer_exit_is_immediate_and_definite() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let a = w.on_process_failed(secs(t0, 30), 1, 1, false);
        assert!(clears(&a));
        assert!(has_log(
            &a,
            "rebuild #1 scheduled in=10s trigger=renderer-gone:RENDER_PROCESS_EXITED"
        ));
        assert!(w.restart.is_some(), "a renderer exit arms the restart");
        assert!(!tries_restart(&a), "but not before a minute has passed");
        let b = w.on_process_failed(secs(t0, 31), 1, 3, false);
        assert_eq!(b.len(), 1);
        assert!(has_log(&b, "FRAME_RENDER_PROCESS_EXITED"));
    }

    #[test]
    fn browser_process_exit_waits_for_the_restart() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let a = w.on_process_failed(secs(t0, 30), 1, 0, false);
        assert!(clears(&a));
        assert!(has_log(&a, "rebuild skipped reason=browser-process-gone"));
        assert!(w.rebuild.is_none());
        assert!(w.restart.is_some());
    }

    #[test]
    fn one_rebuild_in_flight() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let first = w.on_process_failed(secs(t0, 30), 1, 1, false);
        assert_eq!(logs(&first).iter().filter(|m| m.contains("restart pending")).count(), 1);
        let again = w.on_process_failed(secs(t0, 31), 1, 1, false);
        assert!(!has_log(&again, "scheduled"));
        assert!(!has_log(&again, "restart pending"), "armed once");
        let (ticket, acts) = w.on_page_reconnect(secs(t0, 32));
        assert!(ticket.is_none());
        assert!(has_log(&acts, "page reconnect ignored: rebuild pending"));
    }

    #[test]
    fn page_reconnect_immediate_and_attempts_untouched() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.attempts = 2;
        let (ticket, _) = w.on_page_reconnect(secs(t0, 40));
        let id = ticket.expect("a rebuild ticket at once");
        assert!(w.open_step_allowed(id));
        assert_eq!(w.attempts, 2);
        assert!(w.restart.is_none(), "a page-asked rebuild never arms the restart");
    }

    #[test]
    fn page_reconnect_after_signout_finishes_the_signout() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = w.on_signed_out(secs(t0, 10));
        let (ticket, a) = w.on_page_reconnect(secs(t0, 20));
        assert!(ticket.is_none());
        assert!(a.iter().any(|x| matches!(x, Action::FinishSignOut)));
    }

    #[test]
    fn ensure_same_owner_skips_and_expedites_once() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.wanted = Some(OWNER.to_string());
        w.rebuild = Some(test_rebuild(
            9,
            Trigger::Dead(DeadReason::Blank),
            Stage::Waiting { at: secs(t0, 300) },
        ));
        let (go, _) = w.on_ensure(secs(t0, 10), OWNER, Some(OWNER));
        assert!(!go);
        assert_eq!(
            w.rebuild.as_ref().map(|r| r.stage),
            Some(Stage::Waiting { at: secs(t0, 10) })
        );
        let (go2, _) = w.on_ensure(secs(t0, 20), OWNER, Some(OWNER));
        assert!(!go2);
        assert_eq!(
            w.rebuild.as_ref().map(|r| r.stage),
            Some(Stage::Waiting { at: secs(t0, 10) }),
            "expedited once only"
        );
    }

    #[test]
    fn ensure_new_owner_cancels_and_proceeds() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.wanted = Some(OWNER.to_string());
        w.attempts = 3;
        w.rebuild = Some(test_rebuild(
            9,
            Trigger::Dead(DeadReason::Blank),
            Stage::Waiting { at: secs(t0, 300) },
        ));
        let (go, a) = w.on_ensure(secs(t0, 10), OTHER, Some(OWNER));
        assert!(go);
        assert!(w.rebuild.is_none());
        assert_eq!(w.attempts, 0);
        assert!(has_log(&a, "owner=changed"));
    }

    #[test]
    fn ensure_without_rebuild_probes_soon() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = answer(&mut w, secs(t0, 20), &alive(), false);
        let (go, _) = w.on_ensure(secs(t0, 40), OWNER, Some(OWNER));
        assert!(go);
        assert_eq!(w.cur.as_ref().map(|c| c.next_probe_at), Some(secs(t0, 40)));
    }

    #[test]
    fn signout_blocks_rebuild_and_recreate() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = w.on_signed_out(secs(t0, 10));
        let a = w.on_process_failed(secs(t0, 30), 1, 1, true);
        assert!(clears(&a), "the page is gone, so the call flag was stale");
        assert!(a.iter().any(|x| matches!(x, Action::FinishSignOut)));
        assert!(!has_log(&a, "scheduled"));
        assert!(w.restart.is_none());
        // No Phone tab while signed out (the call flag kept the owner): never re-created.
        let s = snap(Some(OWNER), None, false, false);
        let mut all = Vec::new();
        for k in 0..10 {
            all.extend(w.on_tick(secs(t0, 40 + 5 * k), &s));
        }
        assert!(!has_log(&all, "scheduled"));
        // The next sign-in re-arms the watchdog.
        let (go, _) = w.on_ensure(secs(t0, 100), OWNER, None);
        assert!(go);
        let mut after = Vec::new();
        for k in 0..4 {
            after.extend(w.on_tick(secs(t0, 105 + 5 * k), &s));
        }
        assert!(has_log(&after, "reason=missing"));
        assert!(has_log(&after, "scheduled"));
    }

    #[test]
    fn no_owner_is_idle() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.rebuild = Some(test_rebuild(3, Trigger::Missing, Stage::Waiting { at: secs(t0, 50) }));
        let _ = w.on_process_failed(secs(t0, 20), 1, 1, false);
        assert!(w.restart.is_some());
        let a = w.on_tick(secs(t0, 60), &snap(None, Some("tab-1"), true, false));
        assert!(w.rebuild.is_none());
        assert!(w.restart.is_none());
        assert_eq!(rebuilds(&a), 0);
        assert!(probes(&a).is_empty());
        assert!(has_log(&a, "stop reason=no-owner"));
        assert!(has_log(&a, "restart canceled reason=signed-out"));
    }

    #[test]
    fn missing_tab_recreated_after_confirm() {
        let t0 = Instant::now();
        let mut sim = Sim::new(t0, loaded(t0));
        sim.has_tab = false;
        let early = sim.run(5, 10);
        assert!(!has_log(&early, "scheduled"));
        let at15 = sim.tick(15);
        assert!(has_log(&at15, "rebuild #1 scheduled in=10s trigger=missing"));
        assert!(sim.w.restart.is_none(), "a missing tab never arms the restart");
        let created = sim.w.on_webview_created(secs(t0, 20), "tab-2", 2, true);
        assert!(has_log(&created, "rebuild superseded"));
        assert!(sim.w.rebuild.is_none());
    }

    #[test]
    fn stuck_logged_once_after_three() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.attempts = 3;
        let a = w.schedule_rebuild(secs(t0, 10), Trigger::Dead(DeadReason::Blank));
        assert!(has_log(&a, "engine may be stuck"));
        assert!(has_log(&a, "rebuild #4 scheduled in=120s"));
        w.rebuild = None;
        w.attempts = 4;
        let b = w.schedule_rebuild(secs(t0, 20), Trigger::Dead(DeadReason::Blank));
        assert!(!has_log(&b, "engine may be stuck"));
        assert!(has_log(&b, "in=300s"));
    }

    #[test]
    fn alive_during_wait_cancels() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.rebuild = Some(test_rebuild(
            4,
            Trigger::Dead(DeadReason::ErrorPage),
            Stage::Waiting { at: secs(t0, 90) },
        ));
        let a = answer(&mut w, secs(t0, 60), &alive(), false);
        assert!(w.rebuild.is_none());
        assert!(has_log(&a, "came back by itself"));
    }

    #[test]
    fn online_flip_expedites_once() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.rebuild = Some(test_rebuild(
            4,
            Trigger::Dead(DeadReason::ErrorPage),
            Stage::Waiting { at: secs(t0, 300) },
        ));
        w.last_online = Some(false);
        let a = answer(&mut w, secs(t0, 60), &error_page(), false);
        assert!(has_log(&a, "network back"));
        assert_eq!(
            w.rebuild.as_ref().map(|r| r.stage),
            Some(Stage::Waiting { at: secs(t0, 63) })
        );
        w.last_online = Some(false);
        let _ = answer(&mut w, secs(t0, 61), &error_page(), false);
        assert_eq!(
            w.rebuild.as_ref().map(|r| r.stage),
            Some(Stage::Waiting { at: secs(t0, 63) }),
            "once per step"
        );
        // A flip seen before the verdict makes the first wait 3 s.
        let mut v = loaded(t0);
        v.last_online = Some(false);
        let _ = answer(&mut v, secs(t0, 20), &error_page(), false);
        let b = answer(&mut v, secs(t0, 30), &error_page(), false);
        assert!(has_log(&b, "rebuild #1 scheduled in=3s"));
    }

    #[test]
    fn sentinel_navigation_ignored() {
        let t0 = Instant::now();
        let mut w = watched(t0);
        let _ = w.on_nav_starting(1, nav_start(5, t0));
        let mut s = nav_start(6, secs(t0, 30));
        s.sentinel = true;
        assert!(w.on_nav_starting(1, s).is_empty());
        let (started, in_flight) = {
            let c = w.cur.as_ref().expect("cur");
            (c.nav_started, c.nav_in_flight)
        };
        assert_eq!(started, t0);
        assert_eq!(in_flight, Some(5));
        let done = NavDone {
            id: 6,
            at: secs(t0, 30),
            ok: false,
            err: 14,
            http: Some(0),
            source: Some(PageKind::Platform),
        };
        assert!(w.on_nav_completed(secs(t0, 30), 1, done).is_empty());
        assert_eq!(w.cur.as_ref().map(|c| c.nav_in_flight), Some(Some(5)));
    }

    #[test]
    fn navigation_storm_does_not_hide_death() {
        let t0 = Instant::now();
        let mut w = watched(t0);
        for k in 0..10u64 {
            let _ = w.on_nav_starting(1, nav_start(k + 1, secs(t0, 10 * k)));
            let at = secs(t0, 10 * k + 1);
            let _ = w.on_nav_completed(at, 1, nav_done(k + 1, at, false, 11, None));
        }
        let a1 = answer(&mut w, secs(t0, 100), &error_page(), false);
        assert!(!has_log(&a1, "scheduled"));
        // Another reload: in a storm it renews neither the grace nor the strikes.
        let _ = w.on_nav_starting(1, nav_start(11, secs(t0, 105)));
        let _ = w.on_nav_completed(secs(t0, 106), 1, nav_done(11, secs(t0, 106), false, 11, None));
        let a2 = answer(&mut w, secs(t0, 110), &error_page(), false);
        assert!(has_log(&a2, "rebuild #1 scheduled"));
    }

    #[test]
    fn exiting_produces_nothing() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        w.rebuild = Some(test_rebuild(1, Trigger::Missing, Stage::Waiting { at: t0 }));
        let mut s = snap(Some(OWNER), Some("tab-1"), true, false);
        s.exiting = true;
        assert!(w.on_tick(secs(t0, 30), &s).is_empty());
    }

    #[test]
    fn open_step_ticket() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let (ticket, _) = w.on_page_reconnect(secs(t0, 30));
        let id = ticket.expect("ticket");
        assert!(w.open_step_allowed(id));
        assert!(!w.open_step_allowed(id + 1));
        let _ = w.on_signed_out(secs(t0, 31));
        assert!(!w.open_step_allowed(id));
        // A Closing stage that never opened is abandoned after 30 s.
        let mut w2 = loaded(t0);
        w2.rebuild = Some(test_rebuild(7, Trigger::Missing, Stage::Closing { since: secs(t0, 10) }));
        let s = snap(Some(OWNER), Some("tab-1"), true, false);
        assert!(probes(&w2.on_tick(secs(t0, 35), &s)).is_empty(), "no probes while closing");
        let lost = w2.on_tick(secs(t0, 40), &s);
        assert!(has_log(&lost, "rebuild lost id=7"));
        assert!(w2.rebuild.is_none());
    }

    #[test]
    fn page_alive_resets_strikes_and_cancels_wait() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = answer(&mut w, secs(t0, 20), &blank(), false);
        assert_eq!(w.cur.as_ref().map(|c| c.strikes), Some(1));
        w.rebuild = Some(test_rebuild(
            5,
            Trigger::Dead(DeadReason::Blank),
            Stage::Waiting { at: secs(t0, 60) },
        ));
        assert!(w.on_page_alive(secs(t0, 25), "tab-9").is_empty());
        assert!(w.rebuild.is_some());
        let a = w.on_page_alive(secs(t0, 25), "tab-1");
        assert!(has_log(&a, "page alive: rebuild canceled"));
        assert!(w.rebuild.is_none());
        assert_eq!(w.cur.as_ref().map(|c| c.strikes), Some(0));
    }

    #[test]
    fn initial_load_protected_without_nav_start() {
        let t0 = Instant::now();
        let mut w = watched(t0);
        let a = answer(&mut w, secs(t0, 25), &blank(), false);
        assert_eq!(w.cur.as_ref().map(|c| c.strikes), Some(0));
        assert!(has_log(&a, "verdict=loading"));
    }

    #[test]
    fn ambiguous_no_app_held_while_call_flagged() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let no_app = page("p", false, "complete", Some(200)).to_string();
        let _ = answer(&mut w, secs(t0, 20), &no_app, true);
        let a = answer(&mut w, secs(t0, 30), &no_app, true);
        assert!(has_log(&a, "held gen=1 reason=ambiguous-no-app"));
        assert!(!clears(&a));
        assert!(!has_log(&a, "scheduled"));
        assert!(w.restart.is_none());
        // The load balancer's 502 page is unambiguous: the stale flag goes.
        let mut w2 = loaded(t0);
        let _ = answer(&mut w2, secs(t0, 20), &alb502(), true);
        let b = answer(&mut w2, secs(t0, 30), &alb502(), true);
        assert!(clears(&b));
        assert!(has_log(&b, "scheduled"));
    }

    #[test]
    fn still_loading_needs_a_stall() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let loading = |t: u64| set(page("p", false, "interactive", Some(200)), "t", serde_json::json!(t));
        let _ = answer(&mut w, secs(t0, 50), &loading(5), false);
        let a = answer(&mut w, secs(t0, 60), &loading(9), false);
        assert!(!has_log(&a, "scheduled"), "it is still making progress");
        assert_eq!(w.cur.as_ref().map(|c| c.strikes), Some(0));
        let _ = answer(&mut w, secs(t0, 70), &loading(9), false);
        let b = answer(&mut w, secs(t0, 80), &loading(9), false);
        assert!(has_log(&b, "reason=dead:still-loading"));
    }

    #[test]
    fn late_answer_counts_as_alive() {
        let t0 = Instant::now();
        let mut sim = Sim::new(t0, loaded(t0));
        sim.page = None;
        let _ = sim.run(5, 40);
        let (gen, late_id, waiting) = {
            let c = sim.w.cur.as_ref().expect("cur");
            (c.gen, c.late_probe.expect("the timed-out probe is remembered"), c.no_answer_since.is_some())
        };
        assert!(waiting);
        let a = sim
            .w
            .on_probe_answer(secs(t0, 45), gen, late_id, ProbeOutcome::Json(alive()), false);
        assert!(has_log(&a, "probe late"));
        let c = sim.w.cur.as_ref().expect("cur");
        assert!(c.no_answer_since.is_none());
        assert!(c.late_probe.is_none());
        assert_eq!(c.next_probe_at, secs(t0, 105));
    }

    #[test]
    fn late_dead_answer_is_no_evidence() {
        let t0 = Instant::now();
        let mut sim = Sim::new(t0, loaded(t0));
        sim.page = None;
        let _ = sim.run(5, 40);
        let (gen, late_id) = {
            let c = sim.w.cur.as_ref().expect("cur");
            (c.gen, c.late_probe.expect("remembered"))
        };
        let _ = sim
            .w
            .on_probe_answer(secs(t0, 45), gen, late_id, ProbeOutcome::Json(blank()), false);
        let c = sim.w.cur.as_ref().expect("cur");
        assert_eq!(c.strikes, 0);
        assert!(c.no_answer_since.is_none());
    }

    #[test]
    fn error_page_that_stays_silent_is_definite() {
        let t0 = Instant::now();
        let mut w = watched(t0);
        let _ = w.on_nav_starting(1, nav_start(3, t0));
        let c = ContentEvt {
            id: 3,
            at: secs(t0, 1),
            error_page: true,
        };
        let _ = w.on_content_loading(secs(t0, 1), 1, c);
        let _ = w.on_nav_completed(secs(t0, 1), 1, nav_done(3, secs(t0, 1), false, 11, None));
        let mut sim = Sim::new(t0, w);
        sim.page = None;
        sim.call = true; // stale: the old document is gone
        let all = sim.run(5, 75);
        assert!(clears(&all));
        assert!(has_log(&all, "reason=dead:silent:network"));
        assert!(has_log(&all, "scheduled"));
        assert!(sim
            .w
            .restart
            .as_ref()
            .is_some_and(|r| r.evidence == Evidence::Network));
    }

    #[test]
    fn load_that_never_arrived_is_definite() {
        let t0 = Instant::now();
        let mut w = watched(t0);
        let _ = w.on_nav_starting(1, nav_start(1, t0));
        let mut sim = Sim::new(t0, w);
        sim.page = None;
        sim.call = true; // stale: this web view never had a document
        let early = sim.run(5, 70);
        assert!(!clears(&early));
        let all = sim.run(75, 110);
        assert!(clears(&all));
        assert!(has_log(&all, "reason=dead:never-loaded"));
        assert!(tries_restart(&all), "an engine death asks for a restart at once");
    }

    #[test]
    fn engine_death_clears_the_flag_before_the_restart() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = answer(&mut w, secs(t0, 20), &blank(), false);
        let a = answer(&mut w, secs(t0, 30), &blank(), false);
        assert!(has_log(&a, "evidence=engine"));
        let clear_at = a.iter().position(|x| matches!(x, Action::ClearStaleCall));
        let try_at = a.iter().position(|x| matches!(x, Action::TryRestart));
        assert!(clear_at.is_some() && try_at.is_some() && clear_at < try_at);
    }

    #[test]
    fn server_evidence_holds_until_the_server_answers() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = answer(&mut w, secs(t0, 20), &alb502(), false);
        let a = answer(&mut w, secs(t0, 30), &alb502(), false);
        assert!(!tries_restart(&a));
        assert!(has_log(&a, "evidence=server"));
        let s = snap(Some(OWNER), Some("tab-1"), true, false);
        let held = w.on_tick(secs(t0, 35), &s);
        assert!(has_log(&held, "restart held reason=server"));
        assert!(!tries_restart(&held));
        // The rebuilt page loads: the server answers, and the restart may go.
        let _ = w.on_webview_created(secs(t0, 45), "tab-2", 2, true);
        let ok = answer(&mut w, secs(t0, 70), &alive(), false);
        assert!(has_log(&ok, "the server answers"));
        let s2 = snap(Some(OWNER), Some("tab-2"), true, false);
        assert!(tries_restart(&w.on_tick(secs(t0, 70), &s2)));
    }

    #[test]
    fn network_down_holds_the_restart() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = answer(&mut w, secs(t0, 20), &blank(), false);
        let _ = answer(&mut w, secs(t0, 30), &blank(), false);
        assert!(w.restart.is_some());
        w.last_online = Some(false);
        let s = snap(Some(OWNER), Some("tab-1"), true, false);
        let a = w.on_tick(secs(t0, 61), &s);
        assert!(has_log(&a, "restart held reason=network"));
        assert!(!tries_restart(&a));
    }

    #[test]
    fn renderer_exit_waits_a_minute_before_restarting() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = w.on_process_failed(secs(t0, 30), 1, 1, false);
        let s = snap(Some(OWNER), Some("tab-1"), true, false);
        assert!(!tries_restart(&w.on_tick(secs(t0, 60), &s)));
        assert!(tries_restart(&w.on_tick(secs(t0, 90), &s)));
    }

    #[test]
    fn restart_hold_lines_are_throttled() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = w.on_process_failed(secs(t0, 30), 1, 1, false);
        assert!(w.restart_held(secs(t0, 100), "active", " idle=12s").is_some());
        assert!(w.restart_held(secs(t0, 130), "active", " idle=40s").is_none());
        assert!(w.restart_held(secs(t0, 131), "call", "").is_some());
        assert!(w.restart_held(secs(t0, 800), "call", "").is_some());
        let mut idle = Watch::new();
        assert!(idle.restart_held(t0, "active", "").is_none(), "nothing pending");
    }

    #[test]
    fn signout_cancels_the_restart() {
        let t0 = Instant::now();
        let mut w = loaded(t0);
        let _ = w.on_process_failed(secs(t0, 30), 1, 1, false);
        let a = w.on_signed_out(secs(t0, 32));
        assert!(has_log(&a, "restart canceled reason=signed-out"));
        assert!(w.restart.is_none());
    }

    fn armed(t0: Instant) -> Watch {
        let mut w = loaded(t0);
        let _ = w.on_process_failed(secs(t0, 30), 1, 1, false);
        assert!(w.restart.is_some());
        w
    }

    #[test]
    fn ready_signals_cancel_the_restart() {
        let t0 = Instant::now();
        for state in ["available", "call"] {
            let mut w = armed(t0);
            let a = w.on_phone_state(secs(t0, 40), "tab-1", state);
            assert!(has_log(&a, "restart canceled reason=ready"), "{state}");
            assert!(w.restart.is_none(), "{state}");
        }
        // Another tab's word, or "connecting", changes nothing.
        let mut w = armed(t0);
        let _ = w.on_phone_state(secs(t0, 40), "tab-9", "available");
        let _ = w.on_phone_state(secs(t0, 41), "tab-1", "connecting");
        assert!(w.restart.is_some());
    }

    #[test]
    fn offline_cancels_only_with_the_agent_up() {
        let t0 = Instant::now();
        let mut w = armed(t0);
        // A halted page says offline: look at once.
        let a = w.on_phone_state(secs(t0, 40), "tab-1", "offline");
        assert!(has_log(&a, "checking whether the softphone is up"));
        assert_eq!(w.cur.as_ref().map(|c| c.next_probe_at), Some(secs(t0, 40)));
        // No agent: a stuck softphone, the restart stays pending.
        let _ = answer(&mut w, secs(t0, 41), &alive(), false);
        assert!(w.restart.is_some(), "offline with no agent is a stuck softphone");
        // The agent is up (a break, say): the softphone works.
        let up = set(page("p", true, "complete", Some(200)), "a", serde_json::Value::Bool(true));
        let b = answer(&mut w, secs(t0, 50), &up, false);
        assert!(has_log(&b, "restart canceled reason=ready signal=agent"));
        assert!(w.restart.is_none());
    }
}
