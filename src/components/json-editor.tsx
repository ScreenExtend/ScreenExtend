import { forwardRef, memo, useCallback, useEffect, useImperativeHandle, useMemo, useRef } from "react";
import Editor, { type OnMount } from "@monaco-editor/react";
import type * as Monaco from "monaco-editor";
import { monaco, editorOptions, THEME_DARK, THEME_LIGHT } from "@/lib/monaco-setup";
import { scanJson, type JsonStructure, type JsonStringValue } from "@/lib/json-structure";
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

const WORKER_KEEPALIVE_MS = 60_000;

const VALIDATE_IDLE_MS = 900;
const MARKER_OWNER = "screenextend-json";

interface LspPosition {
  line: number;
  character: number;
}

interface LspDiagnostic {
  range: { start: LspPosition; end: LspPosition };
  severity?: number;
  message: string;
  code?: string | number;
  source?: string;
}

interface ValidatingWorker {
  doValidation(uri: string): Promise<LspDiagnostic[]>;
}

const toSeverity = (severity?: number) =>
  severity === 1
    ? monaco.MarkerSeverity.Error
    : severity === 2
      ? monaco.MarkerSeverity.Warning
      : severity === 4
        ? monaco.MarkerSeverity.Hint
        : monaco.MarkerSeverity.Info;

const toMarker = (diagnostic: LspDiagnostic): Monaco.editor.IMarkerData => ({
  severity: toSeverity(diagnostic.severity),
  startLineNumber: diagnostic.range.start.line + 1,
  startColumn: diagnostic.range.start.character + 1,
  endLineNumber: diagnostic.range.end.line + 1,
  endColumn: diagnostic.range.end.character + 1,
  message: diagnostic.message,
  code: typeof diagnostic.code === "number" ? String(diagnostic.code) : diagnostic.code,
  source: diagnostic.source,
});

interface MaskedCache {
  structure: JsonStructure;
  paths: string[];
  entries: { value: JsonStringValue; range: Monaco.Range }[];
}

const ARRAY_INDEX = /\[\d+\]/g;

const LOADING = <div className="text-sm text-muted-foreground">Loading editor…</div>;

const isMasked = (path: string, patterns: string[]) =>
  patterns.includes(path) || patterns.includes(path.replace(ARRAY_INDEX, "[]"));

const schemaRegistry = new Map<string, Record<string, unknown>>();
let appliedSchemas: Map<string, Record<string, unknown>> | null = null;

function schemasUnchanged() {
  if (!appliedSchemas || appliedSchemas.size !== schemaRegistry.size) return false;
  for (const [name, schema] of schemaRegistry) {
    if (appliedSchemas.get(name) !== schema) return false;
  }
  return true;
}

function applySchemas() {
  if (schemasUnchanged()) return;
  appliedSchemas = new Map(schemaRegistry);
  applySchemasNow();
}

function applySchemasNow() {
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


const JsonEditorBase = forwardRef<JsonEditorHandle, JsonEditorProps>(
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
    const maskedCacheRef = useRef<MaskedCache | null>(null);
    const decorationKeyRef = useRef("");

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
      const touch = () => {
        void monaco.languages.json
          .getWorker()
          .then(accessor => accessor())
          .catch(() => {});
      };
      touch();
      const id = window.setInterval(touch, WORKER_KEEPALIVE_MS);
      return () => window.clearInterval(id);
    }, []);

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

    const maskedContent = useCallback(() => {
      const structure = structureRef.current;
      const paths = maskedRef.current;
      const cached = maskedCacheRef.current;
      if (cached && cached.structure === structure && cached.paths === paths) return cached.entries;
      const entries = structure.strings
        .filter(value => isMasked(value.path, paths))
        .map(value => ({
          value,
          range: new monaco.Range(
            value.line,
            value.startColumn + 1,
            value.line,
            value.endColumn - 1
          ),
        }))
        .filter(entry => entry.range.endColumn > entry.range.startColumn);
      maskedCacheRef.current = { structure, paths, entries };
      return entries;
    }, []);

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
      let key = "";
      const selections = editor.getSelections() ?? [];
      const pointer = pointerRef.current;

      for (const { range } of maskedContent()) {
        const revealed =
          selections.some(selection => monaco.Range.areIntersectingOrTouching(selection, range)) ||
          (pointer !== null && range.containsPosition(pointer));
        if (revealed) continue;
        decorations.push({ range, options: { inlineClassName: "se-masked-value" } });
        key += "m" + range.startLineNumber + "," + range.startColumn + "," + range.endColumn + ";";
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
        key += "f" + pair.openLine + "," + column + "," + content + ";";
      }

      if (key === decorationKeyRef.current) return;
      decorationKeyRef.current = key;
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

    const handleMount = useCallback<OnMount>(editor => {
      editorRef.current = editor;
      decorationsRef.current = editor.createDecorationsCollection();

      let validateTimer: number | undefined;
      let validateToken = 0;
      const validate = () => {
        const model = editor.getModel();
        if (!model) return;
        const token = ++validateToken;
        void monaco.languages.json
          .getWorker()
          .then(accessor => accessor(model.uri))
          .then(worker => (worker as unknown as ValidatingWorker).doValidation(model.uri.toString()))
          .then(diagnostics => {
            if (token !== validateToken || editor.getModel() !== model) return;
            monaco.editor.setModelMarkers(model, MARKER_OWNER, diagnostics.map(toMarker));
          })
          .catch(() => {});
      };
      const scheduleValidate = () => {
        window.clearTimeout(validateTimer);
        validateTimer = window.setTimeout(validate, VALIDATE_IDLE_MS);
      };
      editor.onDidBlurEditorText(() => {
        window.clearTimeout(validateTimer);
        validate();
      });
      editor.onDidDispose(() => window.clearTimeout(validateTimer));
      validate();

      const rescan = () => {
        const model = editor.getModel();
        structureRef.current = model ? scanJson(model.getValue()) : EMPTY_STRUCTURE;
        decorationKeyRef.current = "";
        refreshDecorations();
        scheduleValidate();
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
        const previous = pointerRef.current;
        if (next === previous) return;
        if (next && previous && next.lineNumber === previous.lineNumber && next.column === previous.column) {
          return;
        }
        const before = maskAt(previous);
        pointerRef.current = next;
        if (before !== maskAt(next)) refreshDecorations();
      });
      editor.onMouseLeave(() => {
        const wasOver = maskAt(pointerRef.current);
        pointerRef.current = null;
        if (wasOver) refreshDecorations();
      });
      editor.onDidScrollChange(() => {
        if (!pointerRef.current) return;
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
    }, [format, isCollapsed, maskAt, refreshDecorations]);

    const options = useMemo(() => ({ ...editorOptions, readOnly }), [readOnly]);
    const changeRef = useRef(onChange);
    changeRef.current = onChange;
    const handleChange = useCallback((next?: string) => changeRef.current?.(next ?? ""), []);

    return (
      <div className={cn("h-full w-full overflow-hidden", className)}>
        <Editor
          path={uri}
          language="json"
          defaultValue={defaultValue}
          onChange={handleChange}
          onMount={handleMount}
          theme={theme === "dark" ? THEME_DARK : THEME_LIGHT}
          options={options}
          loading={LOADING}
        />
      </div>
    );
  }
);

export const JsonEditor = memo(JsonEditorBase);
