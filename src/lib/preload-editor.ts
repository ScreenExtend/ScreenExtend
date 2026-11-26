// Warms the Settings editor ahead of use: the monaco chunk takes ~2.2 s to import and parse
// in dev on a fast machine. A throwaway off-screen editor fills monaco's global caches (a
// second create measured 8 ms against 99 ms cold); it must use the same `editorOptions` as
// the real one, or the font-measurement cache is keyed differently.
// Everything monaco touches stays behind dynamic imports, or it lands in the main bundle.

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
    const { monaco, editorOptions, THEME_DARK, THEME_LIGHT, assertThemeStylesApplied } =
      await import("@/lib/monaco-setup");

    void monaco.languages.json
      .getWorker()
      .then(accessor => accessor())
      .catch(e => console.error("warming the JSON worker failed", e));

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

    // let it lay out and paint once: that is where the font measuring and first
    // tokenization happen, and the caches it fills are global
    await new Promise(resolve => window.setTimeout(resolve, 0));
    // creating an editor is what makes the theme service write its stylesheet, so the
    // check means something only from here
    assertThemeStylesApplied();
    editor.getModel()?.dispose();
    editor.dispose();
    host.remove();
  } catch (e) {
    // a failed warm-up must never break startup (the editor still loads on demand), but
    // it is logged so a broken monaco chunk is not found only when Advanced is opened
    console.error("warming the settings editor failed", e);
  }
}

// Call once `commands.setup()` has returned and the dashboard is on screen, so the warm-up
// does not overlap virtual-display creation or an early joiner's WebRTC negotiation. The two
// frames are the dashboard's first paint. The `timeout` makes the callback fire within
// PRELOAD_TIMEOUT_MS whether or not the thread goes idle (Safari 13 has no
// requestIdleCallback and gets a plain deadline from legacy-webview-polyfills).
// Not cancelled on unmount: bootstrap navigates away as soon as setup finishes, and
// cancelling there would mean a fast startup never preloads.
export function scheduleConfigEditorPreload(): void {
  if (scheduled) return;
  scheduled = true;
  window.requestAnimationFrame(() => {
    window.requestAnimationFrame(() => {
      requestIdleCallback(() => void preloadConfigEditor(), { timeout: PRELOAD_TIMEOUT_MS });
    });
  });
}
