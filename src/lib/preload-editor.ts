let started: Promise<void> | null = null;

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

    await new Promise(resolve => window.setTimeout(resolve, 0));
    editor.getModel()?.dispose();
    editor.dispose();
    host.remove();
  } catch {}
}

export function scheduleConfigEditorPreload(): void {
  window.setTimeout(() => void preloadConfigEditor(), 300);
}
