import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef } from "react";
import Editor, { type OnMount } from "@monaco-editor/react";
import type * as Monaco from "monaco-editor";
import { monaco, THEME_DARK, THEME_LIGHT } from "@/lib/monaco-setup";
import { scanJson, type JsonStructure } from "@/lib/json-structure";
import { cn } from "@/lib/utils";

export interface JsonMarker {
  message: string;
  severity: "error" | "warning" | "info";
  startLineNumber: number;
  startColumn: number;
  endLineNumber: number;
  endColumn: number;
  source?: string;
}

export interface JsonEditorHandle {
  format(): void;
  minify(): void;
  setValue(next: string): void;
  revealLine(line: number, column?: number): void;
  focus(): void;
  getValue(): string;
  remeasureFonts(): void;
}

export interface JsonEditorProps {
  defaultValue: string;
  onChange?: (value: string) => void;
  schema?: Record<string, unknown> | null;
  name?: string;
  theme?: "light" | "dark";
  maskedValuePaths?: string[];
  readOnly?: boolean;
  className?: string;
  onMarkersChange?: (markers: JsonMarker[]) => void;
}

const modelUri = (name: string) => `inmemory://model/${name}.json`;

const EMPTY_PATHS: string[] = [];
const EMPTY_STRUCTURE: JsonStructure = { pairs: [], strings: [] };

const MAX_FOLD_TAIL = 4;

const ARRAY_INDEX = /\[\d+\]/g;

const isMasked = (path: string, patterns: string[]) =>
  patterns.includes(path) || patterns.includes(path.replace(ARRAY_INDEX, "[]"));

const schemaRegistry = new Map<string, Record<string, unknown>>();

function applySchemas() {
  monaco.languages.json.jsonDefaults.setDiagnosticsOptions({
    validate: true,
    allowComments: false,
    trailingCommas: "error",
    enableSchemaRequest: false,
    schemaValidation: "error",
    schemas: [...schemaRegistry.entries()].map(([name, schema]) => ({
      uri: `inmemory://schema/${name}`,
      fileMatch: [modelUri(name)],
      schema,
    })),
  });
}

const severityOf = (s: Monaco.MarkerSeverity): JsonMarker["severity"] =>
  s === 8 ? "error" : s === 4 ? "warning" : "info";

const editorOptions: Monaco.editor.IStandaloneEditorConstructionOptions = {
  minimap: { enabled: false },
  fontSize: 15,
  lineHeight: 1.7,
  fontFamily:
    'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
  fontLigatures: true,
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
  quickSuggestions: { other: true, strings: true },
  scrollbar: {
    verticalScrollbarSize: 10,
    horizontalScrollbarSize: 10,
    useShadows: false,
    alwaysConsumeMouseWheel: false,
  },
};

export const JsonEditor = forwardRef<JsonEditorHandle, JsonEditorProps>(
  function JsonEditor(
    {
      defaultValue,
      onChange,
      schema,
      name = "document",
      theme = "dark",
      maskedValuePaths,
      readOnly = false,
      className,
      onMarkersChange,
    },
    ref
  ) {
    const editorRef = useRef<Monaco.editor.IStandaloneCodeEditor | null>(null);
    const uri = modelUri(name);
    const maskedRef = useRef(maskedValuePaths ?? EMPTY_PATHS);
    maskedRef.current = maskedValuePaths ?? EMPTY_PATHS;
    const structureRef = useRef<JsonStructure>(EMPTY_STRUCTURE);
    const pointerRef = useRef<Monaco.Position | null>(null);
    const decorationsRef = useRef<Monaco.editor.IEditorDecorationsCollection | null>(null);

    useEffect(() => {
      if (schema) schemaRegistry.set(name, schema);
      else schemaRegistry.delete(name);
      applySchemas();
      return () => {
        schemaRegistry.delete(name);
        applySchemas();
      };
    }, [schema, name]);

    useEffect(() => {
      if (!onMarkersChange) return;
      const publish = () => {
        const model = editorRef.current?.getModel();
        if (!model) return;
        onMarkersChange(
          monaco.editor.getModelMarkers({ resource: model.uri }).map(m => ({
            message: m.message,
            severity: severityOf(m.severity),
            startLineNumber: m.startLineNumber,
            startColumn: m.startColumn,
            endLineNumber: m.endLineNumber,
            endColumn: m.endColumn,
            source: m.source,
          }))
        );
      };
      const sub = monaco.editor.onDidChangeMarkers(resources => {
        if (resources.some(r => r.toString() === uri)) publish();
      });
      publish();
      return () => sub.dispose();
    }, [onMarkersChange, uri]);

    const isCollapsed = useCallback((startLine: number) => {
      const editor = editorRef.current;
      const model = editor?.getModel();
      if (!editor || !model || startLine >= model.getLineCount()) return false;
      return editor.getTopForLineNumber(startLine + 1) <= editor.getTopForLineNumber(startLine);
    }, []);

    const maskedContent = useCallback(
      () =>
        structureRef.current.strings
          .filter(value => isMasked(value.path, maskedRef.current))
          .map(value => ({
            value,
            range: new monaco.Range(
              value.line,
              value.startColumn + 1,
              value.line,
              value.endColumn - 1
            ),
          }))
          .filter(entry => entry.range.endColumn > entry.range.startColumn),
      []
    );

    const maskAt = useCallback(
      (position: Monaco.Position | null) =>
        position
          ? maskedContent().find(entry => entry.range.containsPosition(position))?.value ?? null
          : null,
      [maskedContent]
    );

    const refreshDecorations = useCallback(() => {
      const editor = editorRef.current;
      const model = editor?.getModel();
      const collection = decorationsRef.current;
      if (!editor || !model || !collection) return;

      const decorations: Monaco.editor.IModelDeltaDecoration[] = [];
      const selections = editor.getSelections() ?? [];
      const pointer = pointerRef.current;

      for (const { range } of maskedContent()) {
        const revealed =
          selections.some(selection => monaco.Range.areIntersectingOrTouching(selection, range)) ||
          (pointer !== null && range.containsPosition(pointer));
        if (revealed) continue;
        decorations.push({ range, options: { inlineClassName: "se-masked-value" } });
      }

      for (const pair of structureRef.current.pairs) {
        if (!isCollapsed(pair.openLine)) continue;
        const closer = model.getLineContent(pair.closeLine).trim();
        const content =
          closer.length > 0 && closer.length <= MAX_FOLD_TAIL ? ` ⋯ ${closer}` : " ⋯";
        const column = model.getLineMaxColumn(pair.openLine);
        decorations.push({
          range: new monaco.Range(pair.openLine, column, pair.openLine, column),
          options: {
            after: { content, inlineClassName: "se-fold-tail" },
            showIfCollapsed: true,
          },
        });
      }

      collection.set(decorations);
    }, [isCollapsed, maskedContent]);

    const format = useCallback(() => {
      editorRef.current?.getAction("editor.action.formatDocument")?.run();
    }, []);

    const replaceAll = useCallback((text: string, resetCaret: boolean) => {
      const editor = editorRef.current;
      const model = editor?.getModel();
      if (!editor || !model || model.getValue() === text) return;
      editor.executeEdits("replace-all", [
        { range: model.getFullModelRange(), text },
      ]);
      editor.pushUndoStop();
      if (resetCaret) {
        editor.setPosition({ lineNumber: 1, column: 1 });
        editor.setScrollPosition({ scrollTop: 0 });
      }
    }, []);

    useImperativeHandle(ref, () => ({
      format,
      minify() {
        const current = editorRef.current?.getModel()?.getValue();
        if (!current) return;
        try {
          replaceAll(JSON.stringify(JSON.parse(current)), false);
        } catch {
        }
      },
      setValue: next => replaceAll(next, true),
      revealLine(line, column = 1) {
        const editor = editorRef.current;
        if (!editor) return;
        editor.revealLineInCenter(line);
        editor.setPosition({ lineNumber: line, column });
        editor.focus();
      },
      focus: () => editorRef.current?.focus(),
      getValue: () => editorRef.current?.getModel()?.getValue() ?? "",
      remeasureFonts: () => monaco.editor.remeasureFonts(),
    }));

    const handleMount: OnMount = editor => {
      editorRef.current = editor;
      decorationsRef.current = editor.createDecorationsCollection();

      const rescan = () => {
        const model = editor.getModel();
        structureRef.current = model ? scanJson(model.getValue()) : EMPTY_STRUCTURE;
        refreshDecorations();
      };
      rescan();

      editor.onDidChangeModelContent(rescan);
      editor.onDidChangeCursorSelection(() => refreshDecorations());
      editor.onDidChangeHiddenAreas(() => {
        refreshDecorations();
        requestAnimationFrame(refreshDecorations);
      });
      editor.onMouseMove(event => {
        const next =
          event.target.type === monaco.editor.MouseTargetType.CONTENT_TEXT
            ? event.target.position ?? null
            : null;
        const before = maskAt(pointerRef.current);
        pointerRef.current = next;
        if (before !== maskAt(next)) refreshDecorations();
      });
      editor.onMouseLeave(() => {
        const wasOver = maskAt(pointerRef.current);
        pointerRef.current = null;
        if (wasOver) refreshDecorations();
      });
      editor.onDidScrollChange(() => {
        const wasOver = maskAt(pointerRef.current);
        pointerRef.current = null;
        if (wasOver) refreshDecorations();
      });
      editor.onMouseDown(event => {
        const position = event.target.position;
        const model = editor.getModel();
        if (!position || !model) return;
        if (position.column < model.getLineMaxColumn(position.lineNumber)) return;
        const collapsed = structureRef.current.pairs.some(
          pair => pair.openLine === position.lineNumber && isCollapsed(pair.openLine)
        );
        if (!collapsed) return;
        editor.setPosition({ lineNumber: position.lineNumber, column: 1 });
        void editor.getAction("editor.unfold")?.run();
      });

      editor.addAction({
        id: "json-editor-format",
        label: "Format JSON",
        keybindings: [
          monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyF,
        ],
        run: () => format(),
      });
    };

    return (
      <div className={cn("h-full w-full overflow-hidden", className)}>
        <Editor
          path={uri}
          language="json"
          defaultValue={defaultValue}
          onChange={next => onChange?.(next ?? "")}
          onMount={handleMount}
          theme={theme === "dark" ? THEME_DARK : THEME_LIGHT}
          options={{ ...editorOptions, readOnly }}
          loading={
            <div className="text-sm text-muted-foreground">Loading editor…</div>
          }
        />
      </div>
    );
  }
);
