import "monaco-editor/esm/vs/base/browser/ui/codicons/codiconStyles.js";
import "monaco-editor/esm/vs/editor/common/standaloneStrings.js";
import "monaco-editor/esm/vs/editor/browser/coreCommands.js";
import "monaco-editor/esm/vs/editor/browser/widget/codeEditor/codeEditorWidget.js";
import "monaco-editor/esm/vs/editor/contrib/bracketMatching/browser/bracketMatching.js";
import "monaco-editor/esm/vs/editor/contrib/clipboard/browser/clipboard.js";
import "monaco-editor/esm/vs/editor/contrib/contextmenu/browser/contextmenu.js";
import "monaco-editor/esm/vs/editor/contrib/cursorUndo/browser/cursorUndo.js";
import "monaco-editor/esm/vs/editor/contrib/dnd/browser/dnd.js";
import "monaco-editor/esm/vs/editor/contrib/find/browser/findController.js";
import "monaco-editor/esm/vs/editor/contrib/folding/browser/folding.js";
import "monaco-editor/esm/vs/editor/contrib/format/browser/formatActions.js";
import "monaco-editor/esm/vs/editor/contrib/gotoError/browser/gotoError.js";
import "monaco-editor/esm/vs/editor/contrib/hover/browser/hoverContribution.js";
import "monaco-editor/esm/vs/editor/contrib/lineSelection/browser/lineSelection.js";
import "monaco-editor/esm/vs/editor/contrib/linesOperations/browser/linesOperations.js";
import "monaco-editor/esm/vs/editor/contrib/longLinesHelper/browser/longLinesHelper.js";
import "monaco-editor/esm/vs/editor/contrib/multicursor/browser/multicursor.js";
import "monaco-editor/esm/vs/editor/contrib/readOnlyMessage/browser/contribution.js";
import "monaco-editor/esm/vs/editor/contrib/snippet/browser/snippetController2.js";
import "monaco-editor/esm/vs/editor/contrib/suggest/browser/suggestController.js";
import "monaco-editor/esm/vs/editor/contrib/toggleTabFocusMode/browser/toggleTabFocusMode.js";
import "monaco-editor/esm/vs/editor/contrib/unusualLineTerminators/browser/unusualLineTerminators.js";
import "monaco-editor/esm/vs/editor/contrib/wordOperations/browser/wordOperations.js";
import "monaco-editor/esm/vs/language/json/monaco.contribution";
import * as monaco from "monaco-editor/esm/vs/editor/editor.api";
import { loader } from "@monaco-editor/react";
import { scanJson } from "@/lib/json-structure";
import editorWorker from "@/lib/monaco-editor.worker?worker";
import jsonWorker from "@/lib/monaco-json.worker?worker";

window.MonacoEnvironment = {
  getWorker(_workerId: string, label: string) {
    if (import.meta.env.DEV) {
      const name = label === "json" ? "monaco-json.worker" : "monaco-editor.worker";
      return new Worker("/@classic-worker/" + name + ".js");
    }
    return label === "json" ? new jsonWorker() : new editorWorker();
  },
};

export const THEME_DARK = "screenextend-dark";
export const THEME_LIGHT = "screenextend-light";

monaco.editor.defineTheme(THEME_DARK, {
  base: "vs-dark",
  inherit: true,
  rules: [
    { token: "string.key.json", foreground: "cbd5e1" },
    { token: "string.value.json", foreground: "9db8a6" },
    { token: "number.json", foreground: "c2a98f" },
    { token: "keyword.json", foreground: "a8a3c4" },
    { token: "delimiter", foreground: "64748b" },
    { token: "comment", foreground: "475569", fontStyle: "italic" },
  ],
  colors: {
    "editor.background": "#020817",
    "editor.foreground": "#e2e8f0",
    "editorLineNumber.foreground": "#334155",
    "editorLineNumber.activeForeground": "#94a3b8",
    "editor.lineHighlightBackground": "#ffffff08",
    "editor.selectionBackground": "#33415599",
    "editor.inactiveSelectionBackground": "#1e293b88",
    "editorIndentGuide.background1": "#1e293b",
    "editorIndentGuide.activeBackground1": "#334155",
    "editorBracketMatch.background": "#ffffff10",
    "editorBracketHighlight.foreground1": "#64748b",
    "editorBracketHighlight.foreground2": "#64748b",
    "editorBracketHighlight.foreground3": "#64748b",
    "editorBracketHighlight.foreground4": "#64748b",
    "editorBracketHighlight.foreground5": "#64748b",
    "editorBracketHighlight.foreground6": "#64748b",
    "editorBracketHighlight.unexpectedBracket.foreground": "#f87171",
    "editorBracketMatch.border": "#00000000",
    "editorGutter.background": "#020817",
    "editorWidget.background": "#0f172a",
    "editorWidget.border": "#1e293b",
    "editorSuggestWidget.background": "#0f172a",
    "editorSuggestWidget.border": "#1e293b",
    "editorSuggestWidget.selectedBackground": "#1e293b",
    "editorSuggestWidget.highlightForeground": "#60a5fa",
    "editorHoverWidget.background": "#0f172a",
    "editorHoverWidget.border": "#1e293b",
    "editorError.foreground": "#f87171",
    "editorWarning.foreground": "#e0b054",
    "scrollbarSlider.background": "#33415540",
    "scrollbarSlider.hoverBackground": "#33415570",
    "scrollbarSlider.activeBackground": "#33415590",
  },
});

monaco.editor.defineTheme(THEME_LIGHT, {
  base: "vs",
  inherit: true,
  rules: [
    { token: "string.key.json", foreground: "334155" },
    { token: "string.value.json", foreground: "4e6b52" },
    { token: "number.json", foreground: "8a6236" },
    { token: "keyword.json", foreground: "665c80" },
    { token: "delimiter", foreground: "94a3b8" },
    { token: "comment", foreground: "94a3b8", fontStyle: "italic" },
  ],
  colors: {
    "editor.background": "#ffffff",
    "editor.foreground": "#020817",
    "editorLineNumber.foreground": "#cbd5e1",
    "editorLineNumber.activeForeground": "#475569",
    "editor.lineHighlightBackground": "#00000008",
    "editor.selectionBackground": "#cbd5e1aa",
    "editorIndentGuide.background1": "#f1f5f9",
    "editorIndentGuide.activeBackground1": "#e2e8f0",
    "editorBracketMatch.background": "#0000000d",
    "editorBracketHighlight.foreground1": "#94a3b8",
    "editorBracketHighlight.foreground2": "#94a3b8",
    "editorBracketHighlight.foreground3": "#94a3b8",
    "editorBracketHighlight.foreground4": "#94a3b8",
    "editorBracketHighlight.foreground5": "#94a3b8",
    "editorBracketHighlight.foreground6": "#94a3b8",
    "editorBracketHighlight.unexpectedBracket.foreground": "#dc2626",
    "editorBracketMatch.border": "#00000000",
    "editorGutter.background": "#ffffff",
    "editorWidget.background": "#ffffff",
    "editorWidget.border": "#e2e8f0",
    "editorSuggestWidget.background": "#ffffff",
    "editorSuggestWidget.border": "#e2e8f0",
    "editorSuggestWidget.selectedBackground": "#f1f5f9",
    "editorSuggestWidget.highlightForeground": "#2563eb",
    "editorHoverWidget.background": "#ffffff",
    "editorHoverWidget.border": "#e2e8f0",
    "editorError.foreground": "#dc2626",
    "editorWarning.foreground": "#a16207",
    "scrollbarSlider.background": "#cbd5e140",
    "scrollbarSlider.hoverBackground": "#cbd5e170",
    "scrollbarSlider.activeBackground": "#cbd5e190",
  },
});

monaco.languages.json.jsonDefaults.setModeConfiguration({
  ...monaco.languages.json.jsonDefaults.modeConfiguration,
  foldingRanges: false,
  colors: false,
  diagnostics: false,
  documentSymbols: false,
  selectionRanges: false,
  documentRangeFormattingEdits: false,
});

monaco.languages.registerFoldingRangeProvider("json", {
  provideFoldingRanges(model) {
    return scanJson(model.getValue()).pairs.map(pair => ({
      start: pair.openLine,
      end: pair.closeLine,
    }));
  },
});

export const editorOptions: monaco.editor.IStandaloneEditorConstructionOptions = {
  minimap: { enabled: false },
  fontSize: 14,
  lineHeight: 1.7,
  fontFamily:
    'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
  fontLigatures: false,
  tabSize: 2,
  insertSpaces: true,
  automaticLayout: true,
  scrollBeyondLastLine: false,
  cursorBlinking: "blink",
  renderLineHighlight: "line",
  lineNumbersMinChars: 3,
  glyphMargin: false,
  folding: true,
  padding: { top: 12, bottom: 12 },
  bracketPairColorization: { enabled: false },
  guides: {
    bracketPairs: false,
    bracketPairsHorizontal: false,
    highlightActiveBracketPair: false,
    indentation: true,
    highlightActiveIndentation: false,
  },
  overviewRulerLanes: 0,
  hideCursorInOverviewRuler: true,
  overviewRulerBorder: false,
  stickyScroll: { enabled: false },
  formatOnPaste: true,
  suggest: { showWords: false },
  wordBasedSuggestions: "off",
  accessibilitySupport: "off",
  links: false,
  codeLens: false,
  colorDecorators: false,
  renderControlCharacters: false,
  parameterHints: { enabled: false },
  unicodeHighlight: {
    ambiguousCharacters: false,
    invisibleCharacters: false,
    nonBasicASCII: false,
  },
  quickSuggestions: { other: true, strings: true },
  quickSuggestionsDelay: 500,
  suggestOnTriggerCharacters: false,
  hover: { delay: 800 },
  occurrencesHighlight: "off",
  selectionHighlight: false,
  renderWhitespace: "none",
  scrollbar: {
    verticalScrollbarSize: 10,
    horizontalScrollbarSize: 10,
    useShadows: false,
    alwaysConsumeMouseWheel: false,
  },
};

loader.config({ monaco });

export { monaco };
