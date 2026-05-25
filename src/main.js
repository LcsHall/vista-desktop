// Title-bar window-control wiring.
//
// PLAIN JAVASCRIPT ON PURPOSE. The frontend is served statically from
// src/ with no bundler/build step, so this file is delivered to WebView2
// as-is. A .ts file fails twice over: (1) WebView2's asset server maps
// the .ts extension to the MPEG-TS MIME (video/mp2t), and a
// `type="module"` script with a non-JS MIME is rejected outright; (2)
// TypeScript type syntax isn't valid JS. Either way the module aborts
// and NONE of the button handlers attach — which looked like "the
// min/max/close buttons don't work." Keep this as plain ES.
//
// We use the global `window.__TAURI__` (withGlobalTauri is enabled in
// tauri.conf.json) so we don't need to bundle @tauri-apps/api. The
// window_* commands are defined in src-tauri/src/lib.rs.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`#${id} not found in title bar HTML`);
  return el;
};

const btnMin = $('btn-min');
const btnMax = $('btn-max');
const btnClose = $('btn-close');
const iconMax = $('icon-max');
const iconRestore = $('icon-restore');

btnMin.addEventListener('click', () => invoke('window_minimize').catch(console.error));
btnMax.addEventListener('click', () => invoke('window_toggle_maximize').catch(console.error));
btnClose.addEventListener('click', () => invoke('window_close').catch(console.error));

// Swap the maximize-button icon between "expand" and "restore down"
// when the window's maximize state changes. Poll once on load and
// re-check on Tauri's window-resize event (programmatic maximize/restore
// from our own button click fires it too).
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
