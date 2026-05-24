// Title-bar window-control wiring.
//
// The buttons in index.html fire IPC commands that map to the Rust
// commands exported from src-tauri/src/lib.rs. We use the global
// `window.__TAURI__.core.invoke` rather than importing from
// @tauri-apps/api because withGlobalTauri is enabled in
// tauri.conf.json — saves us from needing a bundler for this tiny
// script. If we ever add real frontend logic here, swap to a proper
// build setup (Vite is the Tauri convention).

// `invoke` and the maximize-state event from Tauri's global runtime.
// The types live in @tauri-apps/api (installed in package.json) but
// are referenced via global so we don't need to bundle.
declare global {
  interface Window {
    __TAURI__: {
      core: {
        invoke: <T = unknown>(cmd: string, args?: Record<string, unknown>) => Promise<T>
      }
      event: {
        listen: (event: string, handler: (payload: { event: string; payload: unknown }) => void) => Promise<() => void>
      }
    }
  }
}

const { invoke } = window.__TAURI__.core
const { listen } = window.__TAURI__.event

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id)
  if (!el) throw new Error(`#${id} not found in title bar HTML`)
  return el as T
}

const btnMin     = $<HTMLButtonElement>('btn-min')
const btnMax     = $<HTMLButtonElement>('btn-max')
const btnClose   = $<HTMLButtonElement>('btn-close')
const iconMax    = $<SVGElement>('icon-max')
const iconRestore= $<SVGElement>('icon-restore')

btnMin.addEventListener('click',   () => invoke('window_minimize').catch(console.error))
btnMax.addEventListener('click',   () => invoke('window_toggle_maximize').catch(console.error))
btnClose.addEventListener('click', () => invoke('window_close').catch(console.error))

// Swap the maximize-button icon between "expand" and "restore down"
// when the window's maximize state changes. We poll once on load and
// re-check on Tauri's window-resize event since there's no first-party
// "maximize-state-changed" event in 2.x at time of writing.
async function refreshMaxIcon(): Promise<void> {
  try {
    const isMax = await invoke<boolean>('window_is_maximized')
    iconMax.style.display     = isMax ? 'none'  : 'block'
    iconRestore.style.display = isMax ? 'block' : 'none'
    btnMax.title              = isMax ? 'Restore Down' : 'Maximize'
    btnMax.setAttribute('aria-label', btnMax.title)
  } catch (e) {
    console.error('[vista-desktop] icon refresh failed:', e)
  }
}

refreshMaxIcon()
// `tauri://resize` fires on every window-size change including
// programmatic maximize/restore from our button click. Poll the
// state after each event to keep the icon in sync.
listen('tauri://resize', refreshMaxIcon).catch(console.error)
