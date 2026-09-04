import "monaco-editor/esm/vs/editor/editor.all.js";
import "monaco-editor/esm/vs/language/json/monaco.contribution";
import * as monaco from "monaco-editor/esm/vs/editor/editor.api";
import { loader } from "@monaco-editor/react";
import { scanJson } from "@/lib/json-structure";
import editorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import jsonWorker from "monaco-editor/esm/vs/language/json/json.worker?worker";

window.MonacoEnvironment = {
  getWorker(_workerId: string, label: string) {
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
});

monaco.languages.registerFoldingRangeProvider("json", {
  provideFoldingRanges(model) {
    return scanJson(model.getValue()).pairs.map(pair => ({
      start: pair.openLine,
      end: pair.closeLine,
    }));
  },
});

loader.config({ monaco });

export { monaco };
