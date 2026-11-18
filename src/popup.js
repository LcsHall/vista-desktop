// Incoming-call pop-up logic. PLAIN JAVASCRIPT ON PURPOSE (no bundler; see
// main.js). Uses the global window.__TAURI__ (withGlobalTauri).
//
// The shell keeps the call that rings here and emits `popup:show` whenever
// it changes (the caller's name can arrive a moment after the ring). Answer
// brings VISTA to the front on the Phone tab and picks up; Ignore stops the
// ringing here for this one call and passes it on (owner 2026-11-18).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`#${id} not found in popup HTML`);
  return el;
};

const nameEl = $('name');
const tagEl = $('tag');
const numEl = $('num');
const timeEl = $('time');
const answerBtn = $('answer');
const ignoreBtn = $('ignore');

let current = null;
let ticker = null;

function clock(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

function render(info) {
  if (!info || typeof info.contactId !== 'string') return;
  const sameCall = current && current.contactId === info.contactId;
  current = info;
  nameEl.textContent = info.name || 'Unknown caller';
  if (info.tag) {
    tagEl.textContent = info.tag;
    tagEl.hidden = false;
  } else {
    tagEl.hidden = true;
  }
  numEl.textContent = info.number && info.number !== info.name ? info.number : '';
  if (!sameCall) {
    // A new call: buttons back on, and the ring clock starts over.
    answerBtn.disabled = false;
    ignoreBtn.disabled = false;
    const started = typeof info.startedAt === 'number' && info.startedAt > 0 ? info.startedAt : Date.now();
    if (ticker) clearInterval(ticker);
    const tick = () => { timeEl.textContent = clock(Date.now() - started); };
    tick();
    ticker = setInterval(tick, 1000);
  }
}

function press(command) {
  if (!current) return;
  answerBtn.disabled = true;
  ignoreBtn.disabled = true;
  invoke(command, { contactId: current.contactId }).catch(console.error);
}

answerBtn.addEventListener('click', () => press('popup_answer'));
ignoreBtn.addEventListener('click', () => press('popup_ignore'));

listen('popup:show', (e) => render(e.payload)).catch(console.error);

// First load: draw the ringing call, then ask the shell to show the window
// (it stays hidden until now, so no empty card flashes up).
(async () => {
  try {
    render(await invoke('popup_current'));
  } catch (e) {
    console.error('[vista-desktop] popup_current failed:', e);
  }
  invoke('popup_ready').catch(console.error);
})();
