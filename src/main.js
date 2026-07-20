// Title-bar logic: tab strip + window controls.
//
// PLAIN JAVASCRIPT ON PURPOSE. The frontend is served statically from
// src/ with no bundler, so this is delivered to WebView2 as-is. A .ts
// file fails twice over (WebView2 serves .ts as the MPEG-TS MIME, which a
// type="module" script rejects, and TS syntax isn't valid JS), so keep
// this plain ES. We use the global window.__TAURI__ (withGlobalTauri is
// enabled in tauri.conf.json) — no need to bundle @tauri-apps/api.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`#${id} not found in title bar HTML`);
  return el;
};

// ── Tabs ─────────────────────────────────────────────────────────────
// Mirrors the Rust-side tab state. The first tab ("Platform") maps to the
// `content` webview and can't be closed. Tabs are added/removed in
// response to events the Rust side emits when /manage/* pages open/close.

const tabsEl = $('tabs');
let tabs = [{ label: 'content', title: 'Platform', closeable: false }];
let active = 'content';

function renderTabs() {
  tabsEl.replaceChildren();
  for (const tab of tabs) {
    const el = document.createElement('div');
    el.className = 'titlebar__tab' +
      (tab.label === active ? ' titlebar__tab--active' : '') +
      (tab.closeable ? '' : ' titlebar__tab--fixed');
    el.setAttribute('role', 'tab');
    el.setAttribute('tabindex', '0');
    el.title = tab.title;

    const label = document.createElement('span');
    label.className = 'titlebar__tab-label';
    label.textContent = tab.title;
    el.appendChild(label);

    el.addEventListener('click', () => selectTab(tab.label));
    el.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); selectTab(tab.label); }
    });

    if (tab.closeable) {
      const close = document.createElement('span');
      close.className = 'titlebar__tab-close';
      close.textContent = '✕';
      close.setAttribute('aria-label', `Close ${tab.title}`);
      close.addEventListener('click', (e) => {
        e.stopPropagation();          // don't also switch to the tab
        invoke('close_tab', { label: tab.label }).catch(console.error);
      });
      el.appendChild(close);
    }

    tabsEl.appendChild(el);
  }
}

// Switch tabs: highlight immediately (responsive), then tell Rust to
// show that tab's webview.
function selectTab(label) {
  if (label === active) return;
  active = label;
  renderTabs();
  invoke('switch_tab', { label }).catch(console.error);
}

// Rust opened a new tab (POS / Messaging / any /manage/* page).
listen('tab:opened', (e) => {
  const { label, title } = e.payload || {};
  if (!label) return;
  if (!tabs.some((t) => t.label === label)) {
    tabs.push({ label, title: title || 'Page', closeable: true });
  }
  active = label;
  renderTabs();
}).catch(console.error);

// Rust closed a tab (via the ✕, or a page's own window.close()).
listen('tab:closed', (e) => {
  const { label, active: nextActive } = e.payload || {};
  tabs = tabs.filter((t) => t.label !== label);
  if (nextActive) active = nextActive;
  renderTabs();
}).catch(console.error);

renderTabs();

// ── Call guard (Vista Voice) ─────────────────────────────────────────
// Rust blocks a window/tab close while a call is live and emits
// call:close-blocked. We overlay a confirm strip on the title bar:
// "Keep call" (or 10s of silence) dismisses; "Close anyway" force-closes
// through the guard — window destroy or forced tab close — hanging up.

const callguardEl = $('callguard');
const callguardMsg = $('callguard-msg');
let callguardTarget = null;    // { kind: 'window' } | { kind: 'tab', label }
let callguardTimer = null;

function hideCallguard() {
  callguardEl.style.display = 'none';
  callguardTarget = null;
  if (callguardTimer) { clearTimeout(callguardTimer); callguardTimer = null; }
}

$('callguard-keep').addEventListener('click', hideCallguard);
$('callguard-close').addEventListener('click', () => {
  const target = callguardTarget;
  hideCallguard();
  if (!target) return;
  if (target.kind === 'window') {
    invoke('window_force_close').catch(console.error);
  } else if (target.label) {
    invoke('close_tab', { label: target.label, force: true }).catch(console.error);
  }
});

listen('call:close-blocked', (e) => {
  const { kind, label } = e.payload || {};
  callguardTarget = { kind: kind || 'window', label };
  callguardMsg.textContent = kind === 'tab'
    ? 'Call in progress — closing the Phone tab will hang up.'
    : 'Call in progress — closing Vista will hang up.';
  callguardEl.style.display = 'flex';
  if (callguardTimer) clearTimeout(callguardTimer);
  callguardTimer = setTimeout(hideCallguard, 10000);
}).catch(console.error);

// ── Window controls ──────────────────────────────────────────────────

const btnMin = $('btn-min');
const btnMax = $('btn-max');
const btnClose = $('btn-close');
const iconMax = $('icon-max');
const iconRestore = $('icon-restore');

btnMin.addEventListener('click', () => invoke('window_minimize').catch(console.error));
btnMax.addEventListener('click', () => invoke('window_toggle_maximize').catch(console.error));
btnClose.addEventListener('click', () => invoke('window_close').catch(console.error));

async function refreshMaxIcon() {
  try {
    const isMax = await invoke('window_is_maximized');
    iconMax.style.display = isMax ? 'none' : 'block';
    iconRestore.style.display = isMax ? 'block' : 'none';
    btnMax.title = isMax ? 'Restore Down' : 'Maximize';
    btnMax.setAttribute('aria-label', btnMax.title);
  } catch (e) {
    console.error('[vista-desktop] icon refresh failed:', e);
  }
}

refreshMaxIcon();
listen('tauri://resize', refreshMaxIcon).catch(console.error);
