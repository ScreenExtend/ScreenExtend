// Opening Settings used to pay for the whole editor at once. Measured in dev on a fast
// machine, that is ~2.2 s to import and parse the monaco chunk (4.3 s the first time),
// ~100 ms for editor.create, and ~620 ms for its first render to settle -- and the 2015
// Intel floor is several times slower again. So do all of it before Settings is opened:
//
//   1. import the chunk        - the dominant cost, paid once
//   2. warm the json worker    - so the first validation is not booting it
//   3. create a throwaway editor off-screen, let it render, dispose it
//
// Step 3 is what makes the real editor cheap: monaco caches font measurements globally per
// font config, initialises its services and tokenizer once, and the render path gets
// JIT-compiled. A second create measured 8 ms against 99 ms cold. It has to use the exact
// same options as the real editor, or the font-measurement cache is keyed differently and
// none of it carries over -- hence the shared `editorOptions`.
//
// Everything monaco touches stays behind dynamic imports; a static one would pull it into
// the main bundle and slow startup for everyone.

const PRELOAD_TIMEOUT_MS = 300;

let started: Promise<void> | null = null;
let scheduled = false;

export function preloadConfigEditor(): Promise<void> {
  if (!started) started = run();
  return started;
}

async function run(): Promise<void> {
  try {
    await import("@/components/config-json-editor");
    const { monaco, editorOptions, THEME_DARK, THEME_LIGHT } = await import("@/lib/monaco-setup");

    void monaco.languages.json
      .getWorker()
      .then(accessor => accessor())
      .catch(() => {});

    const host = document.createElement("div");
    host.setAttribute("aria-hidden", "true");
    host.style.cssText =
      "position:fixed;top:0;left:-10000px;width:800px;height:400px;pointer-events:none";
    document.body.appendChild(host);

    const editor = monaco.editor.create(host, {
      ...editorOptions,
      value: '{\n  "warm": true\n}',
      language: "json",
      theme: document.documentElement.classList.contains("dark") ? THEME_DARK : THEME_LIGHT,
      automaticLayout: false,
    });

    // let it lay out and paint once, which is where the font measuring and first
    // tokenization happen, then throw it away -- the caches it filled are global
    await new Promise(resolve => window.setTimeout(resolve, 0));
    editor.getModel()?.dispose();
    editor.dispose();
    host.remove();
  } catch {
    // a failed warm-up must never break startup; the editor still loads on demand
  }
}

// Called once `commands.setup()` has returned and the dashboard is on screen, so those
// seconds of main-thread scripting no longer overlap virtual-display creation, the config
// reads and any WebRTC negotiation an early joiner starts. The two frames are the
// dashboard's first paint; the idle callback then yields to anything still queued.
//
// The original scheduling was setTimeout(300) from bootstrap mount, and requestIdleCallback
// was rejected then for two reasons: safari 13 does not have it, and waiting for idle is
// exactly how this lost the race against a user opening Settings. Both are answered here --
// the floor gets it from legacy-webview-polyfills, and the `timeout` makes the callback fire
// within PRELOAD_TIMEOUT_MS whether or not the thread ever goes idle, so the deadline is the
// same 300 ms it always was.
//
// Intentionally not cancelled on unmount: bootstrap navigates away as soon as setup
// finishes, and cancelling there would mean a fast startup never preloads at all.
export function scheduleConfigEditorPreload(): void {
  if (scheduled) return;
  scheduled = true;
  window.requestAnimationFrame(() => {
    window.requestAnimationFrame(() => {
      requestIdleCallback(() => void preloadConfigEditor(), { timeout: PRELOAD_TIMEOUT_MS });
    });
  });
}
