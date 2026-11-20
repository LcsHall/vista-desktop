// Headset call button (owner 2026-11-20: "the poly voyager legend 50 has a
// button on it, can that button be used to answer calls?").
//
// Front-desk staff wear Poly Bluetooth headsets. This module lets the
// headset's call button answer and hang up VISTA calls the way Teams and Zoom
// do it. Two routes, both read here in the shell (the Phone tab page cannot
// reach either from inside WebView2):
//
//   * USB headset adapters (Poly BT700 and the like) appear to Windows as a
//     HID "Telephony / Headset" collection. The call button reports Hook
//     Switch (usage page 0x0B, usage 0x20); the host drives the Ring
//     (LED page 0x08, usage 0x18) and Off-Hook (0x17) lights (USB-IF HID
//     Usage Tables 1.5, sections 11 and 14). Read with Windows' own HID API
//     (hid.dll), opened SHARED so Teams or Zoom keep working beside us.
//   * EXPERIMENT (owner "Both", 2026-11-20): a headset paired to Windows' own
//     Bluetooth (the desks' ASUS USB-BT600) has no HID collection; Windows'
//     CallControl API is the only door. Microsoft documents it only with a
//     Windows 8 sample, so it is logged and judged on the owner's test build.
//
// The shell never touches a call. A press goes to the Phone tab
// (window.__VISTA_HEADSET_BUTTON) and the page decides (owner 2026-11-20):
// answer while it rings here, hang up a plain two-person call, pass a
// 2-second press on like Ignore, and do nothing during a transfer or hold.
// The lights follow VISTA's own call state (the RINGING / CALL_ACTIVE signals
// the page already sends). Everything is logged to headset.log in the app's
// local data folder, never a caller's number or name, so a desk's headset can
// be diagnosed from the file alone.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::AppHandle;

// ─── What a press is ─────────────────────────────────────────────────

/// A headset button press, before VISTA's call state gives it a meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// The call button's tap (Hook Switch changed, or the Bluetooth
    /// headset asked to answer or hang up).
    Call,
    /// A dedicated "reject" control (Telephony Line Busy Tone, 0x97): Poly's
    /// 2-second press, if the adapter reports it that way.
    Reject,
    /// Telephony Drop (0x26): "disconnects the active call". Rejects while
    /// ringing, otherwise behaves like the call button.
    Drop,
}

/// VISTA's call state at this desk, as the shell knows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallState {
    /// A call is ringing at this desk (the pop-up's RINGING).
    pub ringing: bool,
    /// A call is live at this desk (CALL_ACTIVE).
    pub active: bool,
}

/// The headset lights VISTA drives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lights {
    pub ring: bool,
    pub off_hook: bool,
    pub mute: bool,
}

/// The lights for a call state. `mute` is only the headset's own mute,
/// echoed back so a headset that waits for the host keeps muting (owner
/// 2026-11-20: mute stays on the headset; VISTA's screen doesn't mirror it).
pub fn lights_for(state: CallState, mute: bool) -> Lights {
    Lights {
        ring: state.ringing && !state.active,
        off_hook: state.active,
        mute: state.active && mute,
    }
}

/// Two presses closer than this are one press: a button's release, a
/// bounce, or the same press seen by two routes.
pub const MIN_GAP: Duration = Duration::from_millis(700);
/// Right after VISTA's call goes live, a headset may briefly report on-hook
/// (the tSIP softphone project saw it on Jabra Link adapters). Not a hang-up.
pub const SETTLE: Duration = Duration::from_millis(1200);
/// The page's call signals come in bursts: a ring starts as "call active" a
/// moment before "ringing", and a missed ring ends the same way round. The
/// headset hears the state once it has held still this long.
#[cfg_attr(not(windows), allow(dead_code))]
pub const STATE_SETTLE: Duration = Duration::from_millis(250);

/// Turns one device's Hook Switch reports into presses.
///
/// Teams-style headsets keep a hook state that follows the Off-Hook light the
/// host sets, and flip it when the person presses. So a change is a press only
/// when the headset now DIFFERS from the light we set; a change that merely
/// catches up with our light (sooner or later, even inside a report about
/// Mute) is not. The worst case is a lost tap, never a call hung up by
/// another button. A momentary (relative) Hook Switch counts its press only,
/// not its release. What a press does is decided by VISTA's call state, never
/// by the bit's value.
#[derive(Debug, Default)]
pub struct HookTracker {
    last: Option<bool>,
    light: bool,
    live_since: Option<Instant>,
    momentary: bool,
}

impl HookTracker {
    /// `momentary`: the Hook Switch is a relative control (press, release).
    pub fn new(momentary: bool) -> Self {
        HookTracker {
            momentary,
            ..Default::default()
        }
    }

    /// What the device is assumed to report right now: on-hook when VISTA
    /// first opens it, so the very first press after the app starts counts.
    pub fn assume(&mut self, off_hook: bool) {
        self.last = Some(off_hook);
    }

    /// The Off-Hook light is now `off_hook`.
    pub fn sent(&mut self, off_hook: bool) {
        self.light = off_hook;
    }

    /// VISTA's call went live (`true`) or is over (`false`).
    pub fn call_live(&mut self, live: bool, now: Instant) {
        self.live_since = if live { Some(now) } else { None };
    }

    /// A report carrying the Hook Switch bit arrived. True when it is a press.
    pub fn report(&mut self, off_hook: bool, now: Instant) -> bool {
        let Some(prev) = self.last else {
            self.last = Some(off_hook);
            return false;
        };
        if prev == off_hook {
            return false; // unchanged: the report was about another control
        }
        if self.momentary {
            self.last = Some(off_hook);
            return off_hook; // the press, not the release
        }
        if off_hook == self.light {
            // The headset caught up with the light we set: nobody pressed.
            self.last = Some(off_hook);
            return false;
        }
        if let Some(at) = self.live_since {
            if !off_hook && now.duration_since(at) < SETTLE {
                // A glitch right after the call went live: keep the state we
                // had, so the headset returning to off-hook isn't a press.
                return false;
            }
        }
        self.last = Some(off_hook);
        true
    }
}

/// One gate for every press from every route and device.
#[derive(Debug, Default)]
pub struct PressGate {
    last: Option<Instant>,
}

impl PressGate {
    pub fn allow(&mut self, now: Instant) -> bool {
        if let Some(at) = self.last {
            if now.duration_since(at) < MIN_GAP {
                return false;
            }
        }
        self.last = Some(now);
        true
    }
}

/// What a press means right now: "call" or "reject", the two kinds the page
/// understands. A Drop rejects while ringing and otherwise acts as the call
/// button.
pub fn press_kind(press: Press, ringing: bool) -> &'static str {
    match press {
        Press::Call => "call",
        Press::Reject => "reject",
        Press::Drop if ringing => "reject",
        Press::Drop => "call",
    }
}

/// The script the shell runs in the Phone tab for a press. The page's own
/// hook decides when it exists (it knows transfers and holds). Pages from
/// before the hook (the web part not deployed yet) still answer and pass on
/// through the pop-up's live hooks; a hang-up needs the page's hook.
pub fn press_script(kind: &str, ringing_contact: Option<&str>) -> String {
    let k = serde_json::Value::String(kind.to_string()).to_string();
    let id = ringing_contact
        .map(|s| serde_json::Value::String(s.to_string()).to_string())
        .unwrap_or_else(|| "null".to_string());
    format!(
        "try{{var w=window,k={k},id={id};\
if(typeof w.__VISTA_HEADSET_BUTTON==='function'){{w.__VISTA_HEADSET_BUTTON(k,id)}}\
else if(id&&k==='call'&&typeof w.__VISTA_ANSWER_CALL==='function'){{w.__VISTA_ANSWER_CALL(id)}}\
else if(id&&k==='reject'&&typeof w.__VISTA_IGNORE_CALL==='function'){{w.__VISTA_IGNORE_CALL(id)}}\
}}catch(e){{}}"
    )
}

// ─── Log (headset.log; no caller details, ever) ─────────────────────

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_LOCK: Mutex<()> = Mutex::new(());
const LOG_MAX_BYTES: u64 = 256 * 1024;

/// One line in headset.log (and on stderr in a dev build).
pub fn log(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    #[cfg(debug_assertions)]
    eprintln!("[headset] {msg}");
    let Some(path) = LOG_PATH.get() else {
        return;
    };
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if fs::metadata(path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "{ms} {msg}");
    }
}

// ─── Wiring ─────────────────────────────────────────────────────────

static GATE: Mutex<PressGate> = Mutex::new(PressGate { last: None });
static LAST_STATE: Mutex<Option<CallState>> = Mutex::new(None);

/// Start both routes. Called once from the app's setup.
pub fn start(app: &AppHandle) {
    use tauri::Manager;
    if let Ok(dir) = app.path().app_local_data_dir() {
        let _ = fs::create_dir_all(&dir);
        let _ = LOG_PATH.set(dir.join("headset.log"));
    }
    log(format!(
        "start: Vista Platform {} (headset button)",
        env!("CARGO_PKG_VERSION")
    ));
    #[cfg(windows)]
    {
        hid::start(app.clone());
        if CALL_CONTROL_EXPERIMENT {
            call_control::start(app.clone());
        }
    }
}

/// Whether the Windows Bluetooth (CallControl) experiment runs. The owner
/// asked for it in the test build (2026-11-20); keep or drop it for the
/// release by what his test shows.
#[cfg(windows)]
const CALL_CONTROL_EXPERIMENT: bool = true;

/// VISTA's call state changed (or may have): bring the headsets' lights and
/// the Bluetooth call indication in step. Cheap when nothing changed — the
/// shell calls it from every pop-up refresh.
pub fn sync(state: CallState) {
    {
        let mut last = LAST_STATE.lock().unwrap_or_else(|e| e.into_inner());
        if *last == Some(state) {
            return;
        }
        *last = Some(state);
    }
    log(format!(
        "state: ringing={} active={}",
        state.ringing, state.active
    ));
    #[cfg(windows)]
    {
        hid::on_state(state);
        if CALL_CONTROL_EXPERIMENT {
            call_control::on_state(state);
        }
    }
}

/// A press arrived from `source`: through the gate, then to the main thread,
/// where the shell gives it to the Phone tab (crate::on_headset_press).
#[cfg_attr(not(windows), allow(dead_code))]
fn fire(app: &AppHandle, press: Press, source: &str) {
    let allowed = GATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow(Instant::now());
    if !allowed {
        log(format!(
            "press {press:?} from {source}: ignored (same press)"
        ));
        return;
    }
    log(format!("press {press:?} from {source}"));
    crate::later(app, move |app| crate::on_headset_press(app, press));
}

// ─── Route 1: USB headset adapters (HID Telephony) ──────────────────

#[cfg(windows)]
mod hid {
    use super::{fire, lights_for, log, CallState, HookTracker, Lights, Press};
    use std::collections::{HashMap, HashSet};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use tauri::AppHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
        CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
    };
    use windows::Win32::Devices::HumanInterfaceDevice::{
        HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetPreparsedData,
        HidD_GetProductString, HidD_SetOutputReport, HidP_GetButtonCaps, HidP_GetCaps,
        HidP_GetUsages, HidP_GetUsagesEx, HidP_InitializeReportForID, HidP_Input,
        HidP_MaxUsageListLength, HidP_Output, HIDD_ATTRIBUTES, HIDP_BUTTON_CAPS, HIDP_CAPS,
        HIDP_REPORT_TYPE, HIDP_STATUS_SUCCESS, PHIDP_PREPARSED_DATA, USAGE_AND_PAGE,
    };
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, NTSTATUS};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    const PAGE_LED: u16 = 0x08;
    const PAGE_TELEPHONY: u16 = 0x0B;
    const TEL_PHONE: u16 = 0x01;
    const TEL_HEADSET: u16 = 0x05;
    const TEL_HOOK_SWITCH: u16 = 0x20;
    const TEL_FLASH: u16 = 0x21;
    const TEL_DROP: u16 = 0x26;
    const TEL_PHONE_MUTE: u16 = 0x2F;
    const TEL_LINE_BUSY_TONE: u16 = 0x97;
    const LED_MUTE: u16 = 0x09;
    const LED_OFF_HOOK: u16 = 0x17;
    const LED_RING: u16 = 0x18;
    const RESCAN: Duration = Duration::from_secs(3);
    /// How long after a press the headset's lights are re-asserted.
    const RESYNC: Duration = Duration::from_millis(2000);

    // HidP_SetUsages writes into the report buffer; windows-rs 0.61 types that
    // buffer as a shared slice, so it gets its own binding with a mutable one.
    #[link(name = "hid", kind = "raw-dylib")]
    extern "system" {
        #[link_name = "HidP_SetUsages"]
        fn hidp_set_usages(
            report_type: HIDP_REPORT_TYPE,
            usage_page: u16,
            link_collection: u16,
            usage_list: *mut u16,
            usage_length: *mut u32,
            preparsed: PHIDP_PREPARSED_DATA,
            report: *mut u8,
            report_length: u32,
        ) -> NTSTATUS;
    }

    /// An owned Win32 handle (closed on drop).
    struct Handle(HANDLE);
    // SAFETY: a file handle is a process-wide kernel object reference; the
    // HID calls made on it are thread-safe.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: we own the handle and close it exactly once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Preparsed report descriptor (freed on drop).
    struct Preparsed(PHIDP_PREPARSED_DATA);
    unsafe impl Send for Preparsed {}
    unsafe impl Sync for Preparsed {}
    impl Drop for Preparsed {
        fn drop(&mut self) {
            // SAFETY: obtained from HidD_GetPreparsedData, freed once.
            unsafe {
                let _ = HidD_FreePreparsedData(self.0);
            }
        }
    }

    /// Where one input usage lives.
    #[derive(Clone, Copy, Debug)]
    struct InputAt {
        report_id: u8,
        absolute: bool,
    }

    /// One output report that carries some of our lights.
    #[derive(Clone, Copy, Debug)]
    struct OutputAt {
        report_id: u8,
        ring: bool,
        off_hook: bool,
        mute: bool,
    }

    struct DevState {
        hook: HookTracker,
        mute: bool,
        mute_bit: bool,
        busy_bit: bool,
        drop_bit: bool,
        flash_bit: bool,
        /// The lights last set (None: never set since opened = all off).
        lights: Option<Lights>,
        state: CallState,
        /// What each input report last showed, so the log keeps only changes.
        logged: HashMap<u8, String>,
    }

    struct Device {
        path: String,
        name: String,
        opened: Instant,
        read: Handle,
        /// None: Windows let us read the headset but not set its lights.
        write: Option<Handle>,
        pre: Preparsed,
        caps: HIDP_CAPS,
        hook: Option<InputAt>,
        mute: Option<InputAt>,
        busy: Option<InputAt>,
        drop: Option<InputAt>,
        flash: Option<InputAt>,
        outputs: Vec<OutputAt>,
        st: Mutex<DevState>,
    }

    static DEVICES: Mutex<Vec<Arc<Device>>> = Mutex::new(Vec::new());
    static TX: OnceLock<Mutex<mpsc::Sender<CallState>>> = OnceLock::new();
    /// The headset's own mute during the current call, by device path, so a
    /// headset re-detected mid-call (unplug, wake) is never un-muted by us.
    static MUTE_BY_PATH: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
    /// Headsets whose reads failed right after opening: left alone a while.
    static COOLDOWN: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

    fn remembered_mute(path: &str) -> bool {
        MUTE_BY_PATH
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|m| m.get(path).copied())
            .unwrap_or(false)
    }

    fn remember_mute(path: &str, mute: Option<bool>) {
        let mut guard = MUTE_BY_PATH.lock().unwrap_or_else(|e| e.into_inner());
        let map = guard.get_or_insert_with(HashMap::new);
        match mute {
            Some(m) => {
                map.insert(path.to_string(), m);
            }
            None => {
                map.remove(path);
            }
        }
    }

    pub fn start(app: AppHandle) {
        let (tx, rx) = mpsc::channel::<CallState>();
        let _ = TX.set(Mutex::new(tx));
        let _ = std::thread::Builder::new()
            .name("headset-hid".into())
            .spawn(move || control(app, rx));
    }

    pub fn on_state(state: CallState) {
        if let Some(tx) = TX.get() {
            let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(state);
        }
    }

    /// The control thread: applies call states to every open headset and
    /// looks for newly plugged-in ones every few seconds.
    fn control(app: AppHandle, rx: mpsc::Receiver<CallState>) {
        let mut current = CallState::default();
        let mut not_headsets: HashSet<String> = HashSet::new();
        let mut warned: HashSet<String> = HashSet::new();
        let mut last_scan: Option<Instant> = None;
        loop {
            match rx.recv_timeout(RESCAN) {
                Ok(mut state) => {
                    // The page's signals arrive in bursts (a ring starts as
                    // "call active" a moment before "ringing"): act on the
                    // state once it has held still for STATE_SETTLE.
                    while let Ok(next) = rx.recv_timeout(super::STATE_SETTLE) {
                        state = next;
                    }
                    if state != current {
                        current = state;
                        let devices = DEVICES.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        for dev in devices {
                            apply(&dev, state);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if last_scan.is_none_or(|t| t.elapsed() >= RESCAN) {
                last_scan = Some(Instant::now());
                scan(&app, &mut not_headsets, &mut warned, current);
            }
        }
    }

    /// Bring one headset's lights in step with VISTA's call state.
    fn apply(dev: &Device, state: CallState) {
        let now = Instant::now();
        let mut st = dev.st.lock().unwrap_or_else(|e| e.into_inner());
        if state != st.state {
            if state.active != st.state.active {
                st.hook.call_live(state.active, now);
            }
            // Every ring and every call starts unmuted.
            st.mute = false;
            remember_mute(&dev.path, None);
        }
        st.state = state;
        let lights = lights_for(state, st.mute);
        write_lights(dev, &mut st, lights, false);
    }

    /// Set the headset's lights. `force` sends them even when unchanged: a
    /// re-assert after a press, so a headset that flipped its own hook state
    /// (a press the page didn't act on) comes back in step with VISTA. While
    /// VISTA is idle and has set nothing, nothing is sent: another app (Teams)
    /// may be using the headset, and an all-off report could cut its call.
    fn write_lights(dev: &Device, st: &mut DevState, lights: Lights, force: bool) {
        let before = st.lights.unwrap_or_default();
        let idle = Lights::default();
        if lights == before && (!force || lights == idle) {
            return;
        }
        let Some(write) = dev.write.as_ref().filter(|_| !dev.outputs.is_empty()) else {
            // No lights to set: VISTA's call state still tells the hook
            // tracker what a "catching up" report would look like.
            st.lights = Some(lights);
            st.hook.sent(lights.off_hook);
            return;
        };
        let len = dev.caps.OutputReportByteLength as usize;
        let mut any_ok = false;
        for out in &dev.outputs {
            let mut buf = vec![0u8; len];
            // SAFETY: buffer sized OutputReportByteLength; preparsed data alive.
            let init = unsafe {
                HidP_InitializeReportForID(HidP_Output, out.report_id, dev.pre.0, &mut buf)
            };
            if init != HIDP_STATUS_SUCCESS {
                log(format!(
                    "{}: output report {} init failed {:#x}",
                    dev.name, out.report_id, init.0
                ));
                continue;
            }
            let mut on: Vec<u16> = Vec::new();
            if out.ring && lights.ring {
                on.push(LED_RING);
            }
            if out.off_hook && lights.off_hook {
                on.push(LED_OFF_HOOK);
            }
            if out.mute && lights.mute {
                on.push(LED_MUTE);
            }
            if !on.is_empty() {
                let mut n = on.len() as u32;
                // SAFETY: valid buffers; the routine writes within `len` bytes.
                let set = unsafe {
                    hidp_set_usages(
                        HidP_Output,
                        PAGE_LED,
                        0,
                        on.as_mut_ptr(),
                        &mut n,
                        dev.pre.0,
                        buf.as_mut_ptr(),
                        len as u32,
                    )
                };
                if set != HIDP_STATUS_SUCCESS {
                    log(format!("{}: set lights failed {:#x}", dev.name, set.0));
                }
            }
            // SAFETY: the write handle is open; buffer is a whole report.
            if unsafe { HidD_SetOutputReport(write.0, buf.as_ptr().cast(), len as u32) } {
                any_ok = true;
                continue;
            }
            let first = windows::core::Error::from_win32().message();
            // Some adapters take output reports only on their interrupt
            // endpoint (SET_REPORT is optional): the same report via WriteFile.
            // SAFETY: synchronous write of a whole report on our own handle.
            match unsafe { WriteFile(write.0, Some(&buf), None, None) } {
                Ok(()) => any_ok = true,
                Err(e) => log(format!(
                    "{}: lights not accepted (report {}): {first}; {}",
                    dev.name,
                    out.report_id,
                    e.message()
                )),
            }
        }
        if !any_ok {
            return; // nothing changed on the headset: keep what we knew
        }
        log(format!(
            "{}: lights ring={} off_hook={} mute={}",
            dev.name, lights.ring, lights.off_hook, lights.mute
        ));
        st.hook.sent(lights.off_hook);
        st.lights = Some(lights);
    }

    /// Every HID interface path present right now.
    fn hid_paths() -> Vec<Vec<u16>> {
        // SAFETY: plain Win32 queries with buffers we size from the API.
        unsafe {
            let guid = HidD_GetHidGuid();
            for _ in 0..3 {
                let mut len = 0u32;
                if CM_Get_Device_Interface_List_SizeW(
                    &mut len,
                    &guid,
                    PCWSTR::null(),
                    CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
                ) != CR_SUCCESS
                    || len <= 1
                {
                    return Vec::new();
                }
                let mut buf = vec![0u16; len as usize];
                if CM_Get_Device_Interface_ListW(
                    &guid,
                    PCWSTR::null(),
                    &mut buf,
                    CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
                ) != CR_SUCCESS
                {
                    continue; // the list changed between the two calls
                }
                return buf
                    .split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        let mut v = s.to_vec();
                        v.push(0);
                        v
                    })
                    .collect();
            }
            Vec::new()
        }
    }

    fn open(path: &[u16], access: u32) -> Option<Handle> {
        // SAFETY: `path` is NUL-terminated UTF-16.
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .ok()
            .map(Handle)
        }
    }

    /// What one HID path turned out to be.
    enum Inspect {
        /// Not a telephony headset collection: never looked at again.
        NotHeadset,
        /// A headset (or a device not ready yet) that could not be opened
        /// now: tried again on the next scan.
        Unavailable,
        Found(Box<Device>),
    }

    fn scan(
        app: &AppHandle,
        not_headsets: &mut HashSet<String>,
        warned: &mut HashSet<String>,
        current: CallState,
    ) {
        let open_paths: HashSet<String> = DEVICES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|d| d.path.clone())
            .collect();
        let cooling: HashSet<String> = {
            let mut guard = COOLDOWN.lock().unwrap_or_else(|e| e.into_inner());
            let map = guard.get_or_insert_with(HashMap::new);
            map.retain(|_, until| *until > Instant::now());
            map.keys().cloned().collect()
        };
        for path in hid_paths() {
            let key = String::from_utf16_lossy(&path[..path.len() - 1]).to_lowercase();
            if open_paths.contains(&key) || not_headsets.contains(&key) || cooling.contains(&key) {
                continue;
            }
            match inspect(&path, &key, warned) {
                Inspect::Found(dev) => {
                    warned.remove(&key);
                    let dev: Arc<Device> = Arc::from(dev);
                    log(describe(&dev));
                    {
                        // Seen mid-call (a replug, a wake): it keeps the mute
                        // the person chose, and the state of the call.
                        let mut st = dev.st.lock().unwrap_or_else(|e| e.into_inner());
                        st.hook.assume(current.active);
                        st.state = current;
                        st.mute = current.active && remembered_mute(&key);
                        if current.active {
                            st.hook.call_live(true, Instant::now());
                            let lights = lights_for(current, st.mute);
                            write_lights(&dev, &mut st, lights, false);
                        } else if current.ringing {
                            let lights = lights_for(current, false);
                            write_lights(&dev, &mut st, lights, false);
                        }
                    }
                    DEVICES
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(dev.clone());
                    let app = app.clone();
                    let _ = std::thread::Builder::new()
                        .name("headset-hid-read".into())
                        .spawn(move || read_loop(app, dev));
                }
                Inspect::NotHeadset => {
                    not_headsets.insert(key);
                }
                Inspect::Unavailable => {}
            }
        }
    }

    /// Open `path` if it is a telephony headset collection.
    fn inspect(path: &[u16], key: &str, warned: &mut HashSet<String>) -> Inspect {
        // Query with no access first: works on every collection, even ones
        // Windows keeps for itself (keyboards, mice), and touches nothing.
        // A failure here may be a device still being set up: try again later.
        let Some(query) = open(path, 0) else {
            return Inspect::Unavailable;
        };
        let mut pre = PHIDP_PREPARSED_DATA(0);
        // SAFETY: valid handle and out-pointer.
        if !unsafe { HidD_GetPreparsedData(query.0, &mut pre) } {
            return Inspect::Unavailable;
        }
        let pre = Preparsed(pre);
        let mut caps = HIDP_CAPS::default();
        // SAFETY: valid preparsed data and out-pointer.
        if unsafe { HidP_GetCaps(pre.0, &mut caps) } != HIDP_STATUS_SUCCESS {
            return Inspect::Unavailable;
        }
        if caps.UsagePage != PAGE_TELEPHONY
            || !(caps.Usage == TEL_HEADSET || caps.Usage == TEL_PHONE)
        {
            return Inspect::NotHeadset;
        }
        let mut attrs = HIDD_ATTRIBUTES {
            Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        // SAFETY: valid handle and out-pointer.
        let _ = unsafe { HidD_GetAttributes(query.0, &mut attrs) };
        let mut name_buf = [0u16; 128];
        // SAFETY: buffer length given in bytes.
        let product = if unsafe {
            HidD_GetProductString(
                query.0,
                name_buf.as_mut_ptr().cast(),
                (name_buf.len() * 2) as u32,
            )
        } {
            let end = name_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(name_buf.len());
            String::from_utf16_lossy(&name_buf[..end])
        } else {
            String::new()
        };
        let name = format!(
            "{} [{:04x}:{:04x}]",
            if product.is_empty() {
                "headset"
            } else {
                product.trim()
            },
            attrs.VendorID,
            attrs.ProductID
        );
        // Two handles: a ReadFile waiting on one would hold up the lights on
        // the same (synchronous) handle. Reading needs read access only; if
        // Windows refuses the write handle, the button still works unlit.
        let Some(read) = open(path, GENERIC_READ.0) else {
            // Logged once per headset; tried again on every scan.
            if warned.insert(key.to_string()) {
                log(format!(
                    "{name}: found, but Windows would not open it (another app may hold it); trying again"
                ));
            }
            return Inspect::Unavailable;
        };
        let write = open(path, GENERIC_READ.0 | GENERIC_WRITE.0);
        if write.is_none() {
            log(format!(
                "{name}: its lights can't be set (no write access); the button still works"
            ));
        }
        let inputs = button_caps(HidP_Input, caps.NumberInputButtonCaps, &pre);
        let outputs = button_caps(HidP_Output, caps.NumberOutputButtonCaps, &pre);
        let find = |page: u16, usage: u16| {
            inputs
                .iter()
                .find(|c| c.UsagePage == page && covers(c, usage))
                .map(|c| InputAt {
                    report_id: c.ReportID,
                    absolute: c.IsAbsolute,
                })
        };
        let mut outs: Vec<OutputAt> = Vec::new();
        for c in outputs.iter().filter(|c| c.UsagePage == PAGE_LED) {
            let at = match outs.iter().position(|o| o.report_id == c.ReportID) {
                Some(i) => i,
                None => {
                    outs.push(OutputAt {
                        report_id: c.ReportID,
                        ring: false,
                        off_hook: false,
                        mute: false,
                    });
                    outs.len() - 1
                }
            };
            outs[at].ring |= covers(c, LED_RING);
            outs[at].off_hook |= covers(c, LED_OFF_HOOK);
            outs[at].mute |= covers(c, LED_MUTE);
        }
        outs.retain(|o| o.ring || o.off_hook || o.mute);
        let hook = find(PAGE_TELEPHONY, TEL_HOOK_SWITCH);
        Inspect::Found(Box::new(Device {
            path: key.to_string(),
            name,
            opened: Instant::now(),
            read,
            write,
            caps,
            hook,
            mute: find(PAGE_TELEPHONY, TEL_PHONE_MUTE),
            busy: find(PAGE_TELEPHONY, TEL_LINE_BUSY_TONE),
            drop: find(PAGE_TELEPHONY, TEL_DROP),
            flash: find(PAGE_TELEPHONY, TEL_FLASH),
            outputs: outs,
            pre,
            st: Mutex::new(DevState {
                hook: HookTracker::new(hook.is_some_and(|h| !h.absolute)),
                mute: false,
                mute_bit: false,
                busy_bit: false,
                drop_bit: false,
                flash_bit: false,
                lights: None,
                state: CallState::default(),
                logged: HashMap::new(),
            }),
        }))
    }

    fn covers(c: &HIDP_BUTTON_CAPS, usage: u16) -> bool {
        // SAFETY: the union member is chosen by IsRange, as the API defines.
        unsafe {
            if c.IsRange {
                (c.Anonymous.Range.UsageMin..=c.Anonymous.Range.UsageMax).contains(&usage)
            } else {
                c.Anonymous.NotRange.Usage == usage
            }
        }
    }

    fn button_caps(kind: HIDP_REPORT_TYPE, n: u16, pre: &Preparsed) -> Vec<HIDP_BUTTON_CAPS> {
        if n == 0 {
            return Vec::new();
        }
        let mut caps = vec![HIDP_BUTTON_CAPS::default(); n as usize];
        let mut len = n;
        // SAFETY: buffer of `n` caps, as HidP_GetCaps reported.
        if unsafe { HidP_GetButtonCaps(kind, caps.as_mut_ptr(), &mut len, pre.0) }
            != HIDP_STATUS_SUCCESS
        {
            return Vec::new();
        }
        caps.truncate(len as usize);
        caps
    }

    fn describe(dev: &Device) -> String {
        let at = |a: Option<InputAt>| match a {
            Some(a) => format!(
                "report {}{}",
                a.report_id,
                if a.absolute { "" } else { " (relative)" }
            ),
            None => "-".to_string(),
        };
        let outs: Vec<String> = dev
            .outputs
            .iter()
            .map(|o| {
                format!(
                    "report {}:{}{}{}",
                    o.report_id,
                    if o.ring { " ring" } else { "" },
                    if o.off_hook { " off-hook" } else { "" },
                    if o.mute { " mute" } else { "" }
                )
            })
            .collect();
        format!(
            "found {} (telephony usage {:#04x}; input {} B, output {} B): hook switch {}, phone mute {}, line busy {}, drop {}, flash {}; lights [{}]{}",
            dev.name,
            dev.caps.Usage,
            dev.caps.InputReportByteLength,
            dev.caps.OutputReportByteLength,
            at(dev.hook),
            at(dev.mute),
            at(dev.busy),
            at(dev.drop),
            at(dev.flash),
            outs.join("; "),
            if dev.write.is_none() { " (lights unavailable)" } else { "" }
        )
    }

    /// One reader per headset, until it is unplugged (or the app exits).
    fn read_loop(app: AppHandle, dev: Arc<Device>) {
        let len = dev.caps.InputReportByteLength as usize;
        let mut buf = vec![0u8; len.max(1)];
        // SAFETY: preparsed data alive for the device's lifetime.
        let max = unsafe { HidP_MaxUsageListLength(HidP_Input, None, dev.pre.0) }.max(1);
        loop {
            let mut n = 0u32;
            // SAFETY: open handle; buffer sized to a whole input report.
            if let Err(e) = unsafe { ReadFile(dev.read.0, Some(&mut buf), Some(&mut n), None) } {
                log(format!("{}: gone ({})", dev.name, e.message()));
                if dev.opened.elapsed() < Duration::from_secs(5) {
                    // Failing straight away: leave it alone for a minute
                    // instead of reopening (and re-logging) it every scan.
                    COOLDOWN
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert_with(HashMap::new)
                        .insert(dev.path.clone(), Instant::now() + Duration::from_secs(60));
                }
                break;
            }
            if n == 0 {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            on_report(&app, &dev, &mut buf[..n as usize], max);
        }
        DEVICES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|d| !Arc::ptr_eq(d, &dev));
    }

    /// The usages pressed in `report` on the telephony page, or None when
    /// this report doesn't carry that page.
    fn pressed(dev: &Device, report: &mut [u8], max: u32) -> Option<Vec<u16>> {
        let mut list = vec![0u16; max as usize];
        let mut n = max;
        // SAFETY: list sized by HidP_MaxUsageListLength; whole input report.
        let st = unsafe {
            HidP_GetUsages(
                HidP_Input,
                PAGE_TELEPHONY,
                None,
                list.as_mut_ptr(),
                &mut n,
                dev.pre.0,
                report,
            )
        };
        if st != HIDP_STATUS_SUCCESS {
            return None;
        }
        list.truncate(n as usize);
        Some(list)
    }

    /// Every pressed usage in the report, any page, for the log.
    fn all_pressed(dev: &Device, report: &[u8], max: u32) -> String {
        let mut list = vec![USAGE_AND_PAGE::default(); max as usize];
        let mut n = max;
        // SAFETY: list sized by HidP_MaxUsageListLength; whole input report.
        let st = unsafe {
            HidP_GetUsagesEx(
                HidP_Input,
                None,
                list.as_mut_ptr(),
                &mut n,
                dev.pre.0,
                report,
            )
        };
        if st != HIDP_STATUS_SUCCESS {
            return format!("(unparsed {:#x})", st.0);
        }
        list[..n as usize]
            .iter()
            .map(|u| format!("{:02x}:{:02x}", u.UsagePage, u.Usage))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn on_report(app: &AppHandle, dev: &Arc<Device>, report: &mut [u8], max: u32) {
        let report_id = report.first().copied().unwrap_or(0);
        let everything = all_pressed(dev, report, max);
        let tel = pressed(dev, report, max);
        let has = |u: u16| tel.as_ref().is_some_and(|t| t.contains(&u));
        let mine =
            |a: Option<InputAt>| tel.is_some() && a.is_some_and(|a| a.report_id == report_id);
        let now = Instant::now();
        // Reject / Drop first: when one report carries both, the gate keeps
        // the first press, and passing a call on must win over answering it.
        let mut presses: Vec<Press> = Vec::new();
        let mut st = dev.st.lock().unwrap_or_else(|e| e.into_inner());
        // The log keeps a report only when it shows something new.
        let changed = st.logged.get(&report_id) != Some(&everything);
        if changed {
            st.logged.insert(report_id, everything.clone());
        }
        let state = st.state;
        if mine(dev.busy) {
            let bit = has(TEL_LINE_BUSY_TONE);
            if bit && !st.busy_bit {
                presses.push(Press::Reject);
            }
            st.busy_bit = bit;
        }
        if mine(dev.drop) {
            let bit = has(TEL_DROP);
            if bit && !st.drop_bit {
                presses.push(Press::Drop);
            }
            st.drop_bit = bit;
        }
        if mine(dev.hook) && st.hook.report(has(TEL_HOOK_SWITCH), now) {
            presses.push(Press::Call);
        }
        let mut flash = false;
        if mine(dev.flash) {
            let bit = has(TEL_FLASH);
            // Poly's 2-second Mute press ("hold" / "switch calls"): logged
            // only. Holding a call from the headset is not a VISTA feature.
            flash = bit && !st.flash_bit;
            st.flash_bit = bit;
        }
        if mine(dev.mute) {
            let bit = has(TEL_PHONE_MUTE);
            // Mute only means something on a live call; between calls it is
            // ignored (every call starts unmuted).
            if st.state.active {
                let absolute = dev.mute.is_some_and(|a| a.absolute);
                let before = st.mute;
                if absolute {
                    st.mute = bit; // an on/off control reports the state itself
                } else if bit && !st.mute_bit {
                    st.mute = !st.mute;
                }
                if st.mute != before {
                    remember_mute(&dev.path, Some(st.mute));
                    // Echo the headset's own mute back to it (owner 2026-11-20:
                    // mute stays on the headset; VISTA's screen doesn't mirror it).
                    let lights = lights_for(st.state, st.mute);
                    write_lights(dev, &mut st, lights, false);
                }
            }
            st.mute_bit = bit;
        }
        if presses.contains(&Press::Call)
            && !presses.contains(&Press::Reject)
            && !presses.contains(&Press::Drop)
            && st.state.ringing
            && !st.state.active
        {
            // Answering: tell the headset at once, as Teams does. A headset
            // waiting for that answer could otherwise drop back on-hook before
            // the call connects, which would read as a second press.
            st.hook.call_live(true, now);
            let lights = Lights {
                ring: false,
                off_hook: true,
                mute: false,
            };
            write_lights(dev, &mut st, lights, false);
        }
        drop(st);
        if changed || !presses.is_empty() {
            log(format!(
                "{}: report {report_id} [{everything}] (ringing={} active={})",
                dev.name, state.ringing, state.active
            ));
        }
        if flash {
            log(format!("{}: flash (hold) pressed: not used", dev.name));
        }
        if presses.is_empty() {
            return;
        }
        // A press the page doesn't act on (a transfer, a hold, nothing going
        // on) may have flipped the headset's own hook state: put its lights
        // back in step with VISTA shortly after. Not while a call rings: an
        // answer or a pass-on is under way, and the next call state syncs.
        let resync = Arc::clone(dev);
        let _ = std::thread::Builder::new()
            .name("headset-hid-resync".into())
            .spawn(move || {
                std::thread::sleep(RESYNC);
                let mut st = resync.st.lock().unwrap_or_else(|e| e.into_inner());
                if st.state.ringing && !st.state.active {
                    return;
                }
                let lights = lights_for(st.state, st.mute);
                write_lights(&resync, &mut st, lights, true);
            });
        for press in presses {
            fire(app, press, "usb headset");
        }
    }
}

// ─── Route 2 (experiment): Windows' own Bluetooth (CallControl) ─────

#[cfg(windows)]
mod call_control {
    use super::{fire, log, CallState, Press};
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};
    use tauri::AppHandle;
    use windows::core::HSTRING;
    use windows::Media::Devices::{
        AudioDeviceRole, CallControl, CallControlEventHandler, DialRequestedEventHandler,
        KeypadPressedEventHandler, MediaDevice, RedialRequestedEventHandler,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    use std::time::Duration;

    static TX: OnceLock<Mutex<mpsc::Sender<CallState>>> = OnceLock::new();

    pub fn start(app: AppHandle) {
        let (tx, rx) = mpsc::channel::<CallState>();
        let _ = TX.set(Mutex::new(tx));
        let _ = std::thread::Builder::new()
            .name("headset-callcontrol".into())
            .spawn(move || run(app, rx));
    }

    pub fn on_state(state: CallState) {
        if let Some(tx) = TX.get() {
            let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(state);
        }
    }

    fn err(e: &windows::core::Error) -> String {
        format!("{:#010x} {}", e.code().0 as u32, e.message())
    }

    /// The Bluetooth hands-free headset Windows uses for calls, if any.
    /// `quiet` skips the per-device lines (the retries after the first look).
    fn find(quiet: bool) -> Option<CallControl> {
        let log = |m: String| {
            if !quiet {
                super::log(m);
            }
        };
        let mut found: Option<CallControl> = None;
        for role in [AudioDeviceRole::Communications, AudioDeviceRole::Default] {
            let name = if role == AudioDeviceRole::Communications {
                "communications"
            } else {
                "default"
            };
            match MediaDevice::GetDefaultAudioRenderId(role) {
                Ok(id) => {
                    match CallControl::FromId(&id) {
                        Ok(cc) => {
                            let ringer = cc
                                .HasRinger()
                                .map(|r| r.to_string())
                                .unwrap_or_else(|e| err(&e));
                            log(format!("callcontrol: {name} speaker device has call control (ringer {ringer})"));
                            if found.is_none() {
                                found = Some(cc);
                            }
                        }
                        Err(e) => log(format!(
                            "callcontrol: {name} speaker device has none ({})",
                            err(&e)
                        )),
                    }
                }
                Err(e) => log(format!(
                    "callcontrol: no {name} speaker device ({})",
                    err(&e)
                )),
            }
        }
        match CallControl::GetDefault() {
            Ok(cc) => {
                log("callcontrol: GetDefault gave one".to_string());
                if found.is_none() {
                    found = Some(cc);
                }
            }
            Err(e) => log(format!("callcontrol: GetDefault gave none ({})", err(&e))),
        }
        found
    }

    /// How often to look again for a Bluetooth headset that wasn't there.
    const RETRY: Duration = Duration::from_secs(15);

    /// Listen to one CallControl. A Bluetooth headset rejects a ring with the
    /// same hang-up command it uses to end a call (Voyager Legend: hold the
    /// call button 2 s, or say "ignore"), so HangUp is a Drop: it passes a
    /// ringing call on and hangs up a live one.
    fn attach(app: &AppHandle, cc: &CallControl) {
        let a = app.clone();
        let _ = cc.AnswerRequested(&CallControlEventHandler::new(move |_| {
            fire(&a, Press::Call, "bluetooth answer");
            Ok(())
        }));
        let a = app.clone();
        let _ = cc.HangUpRequested(&CallControlEventHandler::new(move |_| {
            fire(&a, Press::Drop, "bluetooth hang-up");
            Ok(())
        }));
        let _ = cc.AudioTransferRequested(&CallControlEventHandler::new(|_| {
            log("callcontrol: audio transfer requested (not used)");
            Ok(())
        }));
        let _ = cc.DialRequested(&DialRequestedEventHandler::new(|_, _| {
            log("callcontrol: dial requested (not used)");
            Ok(())
        }));
        let _ = cc.RedialRequested(&RedialRequestedEventHandler::new(|_, _| {
            log("callcontrol: redial requested (not used)");
            Ok(())
        }));
        let _ = cc.KeypadPressed(&KeypadPressedEventHandler::new(|_, _| {
            log("callcontrol: keypad pressed (not used)");
            Ok(())
        }));
        log("callcontrol: listening");
    }

    fn run(app: AppHandle, rx: mpsc::Receiver<CallState>) {
        // SAFETY: initializes COM/WinRT for this thread only (multithreaded).
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let mut prev = CallState::default();
        let mut cc: Option<CallControl> = find(false);
        match &cc {
            Some(c) => attach(&app, c),
            None => log("callcontrol: nothing to use yet (no Bluetooth hands-free headset, or Windows said no); looking again every 15 s and on each ring"),
        }
        let mut token: Option<u64> = None;
        loop {
            // The headset may be off when VISTA starts, or reconnect later:
            // look again every RETRY while there is none.
            let wait = if cc.is_some() {
                Duration::from_secs(3600)
            } else {
                RETRY
            };
            let mut state = match rx.recv_timeout(wait) {
                Ok(s) => s,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cc.is_none() {
                        cc = find(true);
                        if let Some(c) = &cc {
                            attach(&app, c);
                        }
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            // Act once the page's burst of signals has settled (a ring starts
            // as "call active" a moment before "ringing").
            while let Ok(next) = rx.recv_timeout(super::STATE_SETTLE) {
                state = next;
            }
            if state == prev {
                continue;
            }
            if state.ringing && !state.active && !prev.ringing && token.is_none() {
                // A new ring: (re)find the headset if it wasn't usable.
                if cc.is_none() {
                    cc = find(true);
                    if let Some(c) = &cc {
                        attach(&app, c);
                    }
                }
                // No headset ringer: Amazon's ring already plays in the ear.
                if let Some(c) = &cc {
                    match c.IndicateNewIncomingCall(false, &HSTRING::new()) {
                        Ok(t) => {
                            token = Some(t);
                            log("callcontrol: told Windows a call is ringing");
                        }
                        Err(e) => {
                            log(format!(
                                "callcontrol: ringing not accepted ({}); looking again",
                                err(&e)
                            ));
                            cc = None; // stale (the headset went away): look again next ring
                        }
                    }
                }
            }
            if state.active && !prev.active {
                if let Some(c) = &cc {
                    if token.is_none() {
                        token = c.IndicateNewOutgoingCall().ok();
                    }
                    if let Some(t) = token {
                        match c.IndicateActiveCall(t) {
                            Ok(()) => log("callcontrol: told Windows the call is live"),
                            Err(e) => {
                                log(format!("callcontrol: live call not accepted ({})", err(&e)))
                            }
                        }
                    }
                }
            }
            if !state.ringing && !state.active {
                if let (Some(c), Some(t)) = (&cc, token.take()) {
                    match c.EndCall(t) {
                        Ok(()) => log("callcontrol: told Windows the call is over"),
                        Err(e) => log(format!("callcontrol: end not accepted ({})", err(&e))),
                    }
                }
                token = None;
            }
            prev = state;
        }
    }
}

// ─── Tests (the pure parts; the Win32 I/O needs a real headset) ─────

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn first_report_after_assume_counts() {
        let t0 = Instant::now();
        let mut h = HookTracker::new(false);
        h.assume(false);
        assert!(h.report(true, at(t0, 0)), "idle → off-hook is a press");
    }

    #[test]
    fn unknown_start_never_presses() {
        let t0 = Instant::now();
        let mut h = HookTracker::new(false);
        assert!(
            !h.report(true, at(t0, 0)),
            "first report only teaches the state"
        );
        assert!(
            !h.report(false, at(t0, 50)),
            "back to the light (on-hook) is catching up, not a press"
        );
        assert!(h.report(true, at(t0, 100)), "away from the light counts");
    }

    #[test]
    fn unchanged_bit_is_not_a_press() {
        let t0 = Instant::now();
        let mut h = HookTracker::new(false);
        h.assume(false);
        assert!(!h.report(false, at(t0, 10)));
        assert!(h.report(true, at(t0, 20)));
        assert!(
            !h.report(true, at(t0, 30)),
            "a report about another control"
        );
    }

    #[test]
    fn catching_up_with_our_light_is_never_a_press_however_late() {
        let t0 = Instant::now();
        let mut h = HookTracker::new(false);
        h.assume(true); // on a call; the caller hangs up
        h.sent(false);
        assert!(
            !h.report(false, at(t0, 30_000)),
            "the headset following Off-Hook = 0, even 30 s later"
        );
        // A real press later is a press again.
        assert!(h.report(true, at(t0, 40_000)));
    }

    #[test]
    fn a_mute_press_never_hangs_up() {
        // Reviewer's case: VISTA set Off-Hook (call answered on screen); a
        // Teams-style adapter went off-hook without a report. 40 s later the
        // receptionist presses Mute, and that report carries Hook = 1.
        let t0 = Instant::now();
        let mut h = HookTracker::new(false);
        h.assume(false);
        h.call_live(true, at(t0, 0));
        h.sent(true);
        assert!(
            !h.report(true, at(t0, 40_000)),
            "hook = 1 inside a Mute report only catches up with our light"
        );
        assert!(h.report(false, at(t0, 50_000)), "a real tap still hangs up");
    }

    #[test]
    fn on_hook_flicker_right_after_answer_is_ignored() {
        let t0 = Instant::now();
        let mut h = HookTracker::new(false);
        h.assume(false);
        assert!(h.report(true, at(t0, 0)), "the answering press");
        h.call_live(true, at(t0, 300));
        h.sent(true);
        assert!(!h.report(false, at(t0, 600)), "flicker within SETTLE");
        assert!(
            !h.report(true, at(t0, 800)),
            "the headset returning to off-hook after the glitch is not a hang-up"
        );
        assert!(
            h.report(false, at(t0, 6000)),
            "a real press later still counts"
        );
    }

    #[test]
    fn hang_up_press_after_settle_counts() {
        let t0 = Instant::now();
        let mut h = HookTracker::new(false);
        h.assume(true);
        h.call_live(true, at(t0, 0));
        h.sent(true);
        assert!(h.report(false, at(t0, 5000)), "a press during the call");
    }

    #[test]
    fn headset_that_ignores_lights_loses_at_most_one_tap() {
        // A toggle-style headset that never follows the host's light: after a
        // call the caller ended, its hook bit is still "off-hook".
        let t0 = Instant::now();
        let mut h = HookTracker::new(false);
        h.assume(false);
        assert!(h.report(true, at(t0, 0)), "answers");
        h.sent(true);
        h.sent(false); // the caller hung up; the headset kept hook = 1
        assert!(
            !h.report(false, at(t0, 60_000)),
            "first tap of the next ring is lost"
        );
        assert!(h.report(true, at(t0, 61_000)), "the second one answers");
    }

    #[test]
    fn momentary_hook_counts_the_press_not_the_release() {
        let t0 = Instant::now();
        let mut h = HookTracker::new(true);
        h.assume(false);
        assert!(h.report(true, at(t0, 0)), "press");
        assert!(!h.report(false, at(t0, 2000)), "release, even 2 s later");
        assert!(h.report(true, at(t0, 9000)), "the next press");
    }

    #[test]
    fn gate_merges_close_presses() {
        let t0 = Instant::now();
        let mut g = PressGate::default();
        assert!(g.allow(at(t0, 0)));
        assert!(!g.allow(at(t0, 200)), "release / bounce / second route");
        assert!(g.allow(at(t0, 900)));
    }

    #[test]
    fn lights_follow_call_state() {
        let ringing = CallState {
            ringing: true,
            active: false,
        };
        let live = CallState {
            ringing: false,
            active: true,
        };
        let idle = CallState::default();
        assert_eq!(
            lights_for(ringing, false),
            Lights {
                ring: true,
                off_hook: false,
                mute: false
            }
        );
        assert_eq!(
            lights_for(live, true),
            Lights {
                ring: false,
                off_hook: true,
                mute: true
            }
        );
        assert_eq!(
            lights_for(idle, true),
            Lights::default(),
            "mute never shows off a call"
        );
        // Never produced by the shell (live = CALL_ACTIVE && !ringing), but if
        // both were ever set the live call wins: no Ring light on a live call.
        assert_eq!(
            lights_for(
                CallState {
                    ringing: true,
                    active: true
                },
                false
            ),
            Lights {
                ring: false,
                off_hook: true,
                mute: false
            }
        );
    }

    #[test]
    fn drop_means_reject_only_while_ringing() {
        assert_eq!(press_kind(Press::Drop, true), "reject");
        assert_eq!(press_kind(Press::Drop, false), "call");
        assert_eq!(press_kind(Press::Call, true), "call");
        assert_eq!(press_kind(Press::Reject, false), "reject");
    }

    #[test]
    fn press_script_quotes_and_falls_back() {
        let s = press_script("call", Some("c-1\"x"));
        assert!(s.contains(r#"k="call""#));
        assert!(
            s.contains(r#"id="c-1\"x""#),
            "contact id is JSON-escaped: {s}"
        );
        assert!(s.contains("__VISTA_HEADSET_BUTTON(k,id)"));
        assert!(s.contains("__VISTA_ANSWER_CALL(id)"));
        assert!(s.contains("__VISTA_IGNORE_CALL(id)"));
        let idle = press_script("call", None);
        assert!(idle.contains("id=null"));
    }
}
