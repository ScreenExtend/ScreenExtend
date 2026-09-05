import { useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Braces,
  CheckCircle2,
  Copy,
  Loader2,
  Minimize2,
  RotateCcw,
  Save,
  X,
  ShieldCheck,
  WrapText,
  XCircle,
} from "lucide-react";

import {
  JsonEditor,
  type JsonEditorHandle,
  type JsonMarker,
} from "@/components/json-editor";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useToast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { getConfig, getConfigSnapshot, subscribeToConfig, type Config } from "@/components/config-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { configJsonSchema, validateConfig } from "@/lib/config-schema";
import { applyConfig } from "@/lib/apply-config";

interface Conflict {
  mine: string;
  theirs: string;
}

interface Problem {
  message: string;
  severity: "error" | "warning" | "info";
  line?: number;
  column?: number;
}

interface ConfigJsonEditorProps {
  minHostedNetworkPasswordLength: number;
  onDirtyChange?: (dirty: boolean) => void;
  onSaved?: (config: Config) => void | Promise<void>;
}

const serialize = (config: Config) => JSON.stringify(config, null, 2) + "\n";

const MASKED_PATHS = [
  "hostedNetworkCredentials.password",
  "devices[].token",
  "knownDevices[].token",
  "turnConfig.credential",
];

const formatBytes = (n: number) =>
  n < 1024 ? `${n} B` : `${(n / 1024).toFixed(1)} KB`;

function describeParseError(error: unknown, text: string): Problem {
  const message = error instanceof Error ? error.message : String(error);
  const withLine = /line (\d+) column (\d+)/i.exec(message);
  if (withLine) {
    return { message, severity: "error", line: Number(withLine[1]), column: Number(withLine[2]) };
  }
  const withPosition = /position (\d+)/i.exec(message);
  if (withPosition) {
    const offset = Math.min(Number(withPosition[1]), text.length);
    const before = text.slice(0, offset);
    const line = before.split("\n").length;
    return {
      message,
      severity: "error",
      line,
      column: offset - before.lastIndexOf("\n"),
    };
  }
  return { message, severity: "error" };
}

export function ConfigJsonEditor({
  minHostedNetworkPasswordLength,
  onDirtyChange,
  onSaved,
}: ConfigJsonEditorProps) {
  const { toast } = useToast();
  const { t } = useTranslation();
  const { windowZoom: [zoom] } = useContext(GlobalProviderContext);

  const editorRef = useRef<JsonEditorHandle>(null);
  const seed = useRef<string | null>(null);
  const textRef = useRef("");
  const baseRef = useRef("");

  const [text, setText] = useState("");
  const [savedText, setSavedText] = useState("");
  const [markers, setMarkers] = useState<JsonMarker[]>([]);
  const [saveErrors, setSaveErrors] = useState<Problem[]>([]);
  const [saving, setSaving] = useState(false);
  const [copied, setCopied] = useState(false);
  const [ready, setReady] = useState(false);
  const [loadError, setLoadError] = useState(false);
  const [conflict, setConflict] = useState<Conflict | null>(null);
  const [conflictKey, setConflictKey] = useState(0);
  const [theme, setTheme] = useState<"light" | "dark">(() =>
    document.documentElement.classList.contains("dark") ? "dark" : "light"
  );

  useEffect(() => {
    const observer = new MutationObserver(() =>
      setTheme(document.documentElement.classList.contains("dark") ? "dark" : "light")
    );
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const id = setTimeout(() => editorRef.current?.remeasureFonts(), 150);
    return () => clearTimeout(id);
  }, [zoom]);

  const load = useCallback(async () => {
    setLoadError(false);
    try {
      const config = await getConfig();
      if (!config) throw new Error("config store is empty");
      const initial = serialize(config);
      seed.current = initial;
      setText(initial);
      setSavedText(initial);
      setReady(true);
    } catch {
      setLoadError(true);
      toast({
        variant: "destructive",
        title: t("toasts.configEditor.loadFailedTitle"),
        description: t("toasts.configEditor.loadFailedDescription"),
      });
    }
  }, [t, toast]);

  useEffect(() => {
    void load();
  }, [load]);

  textRef.current = text;
  baseRef.current = savedText;

  const dirty = ready && text !== savedText;
  useEffect(() => {
    onDirtyChange?.(dirty);
  }, [dirty, onDirtyChange]);

  const adopt = useCallback((next: string) => {
    setSavedText(next);
    setText(next);
    setSaveErrors([]);
    baseRef.current = next;
    textRef.current = next;
    editorRef.current?.setValue(next);
  }, []);

  useEffect(
    () =>
      subscribeToConfig(() => {
        const config = getConfigSnapshot();
        if (!config) return;
        const next = serialize(config);
        if (next === baseRef.current) return;
        if (textRef.current !== baseRef.current) return;
        adopt(next);
      }),
    [adopt]
  );

  const onChange = useCallback((next: string) => {
    setText(next);
    setSaveErrors([]);
  }, []);

  const problems = useMemo<Problem[]>(() => {
    const rows: Problem[] = markers.map(m => ({
      message: m.message,
      severity: m.severity,
      line: m.startLineNumber,
      column: m.startColumn,
    }));
    for (const error of saveErrors) {
      if (!rows.some(row => row.message === error.message)) rows.push(error);
    }
    return rows;
  }, [markers, saveErrors]);

  const stats = useMemo(() => {
    let lineCount = 1;
    let bytes = 0;
    for (let i = 0; i < text.length; i++) {
      const code = text.charCodeAt(i);
      if (code === 10) lineCount++;
      if (code < 0x80) bytes += 1;
      else if (code < 0x800) bytes += 2;
      else if (code >= 0xd800 && code <= 0xdbff) {
        bytes += 4;
        i++;
      } else bytes += 3;
    }
    return { lines: lineCount, bytes };
  }, [text]);

  const errorCount = problems.filter(p => p.severity === "error").length;
  const warningCount = problems.filter(p => p.severity === "warning").length;

  const revert = () => {
    setSaveErrors([]);
    setText(savedText);
    editorRef.current?.setValue(savedText);
  };

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(editorRef.current?.getValue() ?? text);
    } catch {
      toast({
        variant: "destructive",
        title: t("toasts.configEditor.copyFailedTitle"),
        description: t("toasts.configEditor.copyFailedDescription"),
      });
      return;
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 1400);
  };

  const fail = (errors: Problem[], title: string, description: string) => {
    setSaveErrors(errors);
    toast({ variant: "destructive", title, description });
  };

  const commit = async () => {
    const raw = editorRef.current?.getValue() ?? text;
    setSaving(true);
    try {
      let parsed: unknown;
      try {
        parsed = JSON.parse(raw);
      } catch (e) {
        const problem = describeParseError(e, raw);
        fail(
          [problem],
          t("toasts.configEditor.parseFailedTitle"),
          problem.line
            ? t("toasts.configEditor.parseFailedAt", {
                line: problem.line,
                column: problem.column ?? 1,
                message: problem.message,
              })
            : problem.message
        );
        return;
      }

      const invalid = validateConfig(parsed, { minHostedNetworkPasswordLength });
      if (invalid.length > 0) {
        fail(
          invalid.map(e => ({ message: e.message, severity: "error" as const })),
          t("toasts.configEditor.invalidTitle"),
          invalid.length === 1
            ? invalid[0].message
            : t("toasts.configEditor.invalidDescription", {
                count: invalid.length,
                first: invalid[0].message,
              })
        );
        return;
      }

      const applied = await applyConfig(parsed as Config);
      adopt(serialize(getConfigSnapshot() ?? applied));
      toast({
        title: t("toasts.configEditor.savedTitle"),
        description: t("toasts.configEditor.savedDescription"),
      });
      try {
        await onSaved?.(applied);
      } catch (e) {
        console.error("config refresh after save failed", e);
      }
    } catch (e) {
      const storeWrite = e instanceof Error && e.message.startsWith("config.json:");
      fail(
        [{ message: String(e), severity: "error" }],
        storeWrite ? t("toasts.config.saveFailedTitle") : t("toasts.configEditor.applyFailedTitle"),
        storeWrite
          ? t("toasts.config.saveFailedDescription")
          : t("toasts.configEditor.applyFailedDescription")
      );
    } finally {
      setSaving(false);
    }
  };

  const save = async () => {
    const config = getConfigSnapshot();
    const stored = config ? serialize(config) : null;
    if (stored !== null && stored !== baseRef.current) {
      setConflict({ mine: editorRef.current?.getValue() ?? text, theirs: stored });
      setConflictKey(key => key + 1);
      return;
    }
    await commit();
  };

  const resolveWithMine = async () => {
    const stored = conflict?.theirs ?? null;
    setConflict(null);
    if (stored !== null) baseRef.current = stored;
    await commit();
  };

  const resolveWithStored = () => {
    if (conflict) adopt(conflict.theirs);
    setConflict(null);
  };

  return (
    <div className="flex flex-col">
      <TooltipProvider delay={150}>
        <div className="flex items-center justify-between space-x-2 rounded-t-md border border-b-0 bg-muted/40 px-2 py-2">
          <div className="flex items-center space-x-2 pl-1">
            <Braces className="h-4 w-4 text-muted-foreground" />
            <span className="text-sm font-medium">config.json</span>
            {dirty && (
              <span className="text-xs text-muted-foreground">
                {t("configEditor.unsavedBadge")}
              </span>
            )}
          </div>
          <div className="flex items-center space-x-0.5">
            <ToolButton
              label={t("configEditor.actions.format")}
              onClick={() => editorRef.current?.format()}
            >
              <WrapText className="h-4 w-4" />
            </ToolButton>
            <ToolButton
              label={t("configEditor.actions.minify")}
              onClick={() => editorRef.current?.minify()}
            >
              <Minimize2 className="h-4 w-4" />
            </ToolButton>
            <div className="mx-1 h-5 w-px bg-border" />
            <ToolButton
              label={copied ? t("configEditor.actions.copied") : t("configEditor.actions.copy")}
              onClick={() => void copy()}
            >
              {copied ? <CheckCircle2 className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
            </ToolButton>
            <ToolButton
              label={t("configEditor.actions.revert")}
              onClick={revert}
              disabled={!dirty}
            >
              <RotateCcw className="h-4 w-4" />
            </ToolButton>
          </div>
        </div>
      </TooltipProvider>

      <div className="h-[420px] border bg-background">
        {ready && seed.current !== null ? (
          <JsonEditor
            ref={editorRef}
            name="screenextend-config"
            defaultValue={seed.current}
            onChange={onChange}
            schema={configJsonSchema}
            maskedValuePaths={MASKED_PATHS}
            theme={theme}
            onMarkersChange={setMarkers}
          />
        ) : loadError ? (
          <div className="flex h-full items-center justify-center px-6 text-center text-sm text-muted-foreground">
            <XCircle className="mr-2 h-4 w-4 shrink-0 text-destructive" />
            {t("toasts.configEditor.loadFailedDescription")}
          </div>
        ) : (
          <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
            <Loader2 className="mr-2 h-4 w-4 animate-spin" />
            {t("configEditor.loading")}
          </div>
        )}
      </div>

      {problems.length > 0 && (
        <div className="border border-t-0">
          <div className="px-3 py-2 text-sm font-medium text-muted-foreground">
            {t("configEditor.problems")}
          </div>
          <ul className="max-h-[150px] overflow-y-auto pb-1">
            {problems.map((problem, i) => (
              <li key={i}>
                <button
                  type="button"
                  onClick={() =>
                    problem.line && editorRef.current?.revealLine(problem.line, problem.column)
                  }
                  className="flex w-full items-start space-x-2.5 px-3 py-2 text-left text-sm transition-colors hover:bg-accent"
                >
                  {problem.severity === "error" ? (
                    <XCircle className="mt-0.5 h-4 w-4 shrink-0 text-destructive" />
                  ) : (
                    <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-yellow-500" />
                  )}
                  <span className="flex-1 break-words">{problem.message}</span>
                  {problem.line && (
                    <span className="font-mono tabular-nums text-muted-foreground">
                      {problem.line}:{problem.column ?? 1}
                    </span>
                  )}
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="flex flex-wrap items-center rounded-b-md border border-t-0 bg-muted/40 px-3 pb-2 text-sm text-muted-foreground [&>*:not(:last-child)]:mr-4 [&>*]:mt-2">
        {errorCount === 0 && warningCount === 0 ? (
          <Badge variant="outline" className="font-normal">
            <CheckCircle2 className="mr-1.5 h-3.5 w-3.5" />
            {t("configEditor.status.valid")}
          </Badge>
        ) : (
          <Badge variant="destructive" className="font-normal">
            <XCircle className="mr-1.5 h-3.5 w-3.5" />
            {errorCount > 0
              ? t("configEditor.status.errors", { count: errorCount })
              : t("configEditor.status.warnings", { count: warningCount })}
          </Badge>
        )}
        <span className="flex items-center">
          <ShieldCheck className="mr-1.5 h-4 w-4" />
          {t("configEditor.status.schema")}
        </span>
        <span className="font-mono tabular-nums">
          {stats.lines} lines, {formatBytes(stats.bytes)}
        </span>
        <div className="ml-auto">
          <Button onClick={() => void save()} disabled={saving || !ready}>
            {saving ? (
              <Loader2 className="mr-2 h-4 w-4 animate-spin" />
            ) : (
              <Save className="mr-2 h-4 w-4" />
            )}
            {t("configEditor.actions.save")}
          </Button>
        </div>
      </div>

      <AlertDialog
        open={conflict !== null}
        onOpenChange={open => { if (!open) setConflict(null); }}
      >
        <AlertDialogContent className="w-[96vw] max-w-[96vw] gap-4">
          <AlertDialogHeader>
            <AlertDialogTitle>{t("configEditor.conflict.title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("configEditor.conflict.description")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <Button
            variant="ghost"
            size="icon"
            className="absolute right-4 top-4 h-8 w-8"
            onClick={() => setConflict(null)}
          >
            <X className="h-4 w-4" />
            <span className="sr-only">{t("common.back")}</span>
          </Button>
          {conflict && (
            <div className="grid gap-4 md:grid-cols-2">
              <ComparePane
                title={t("configEditor.conflict.mine")}
                caption={t("configEditor.conflict.mineCaption")}
                name={`config-conflict-mine-${conflictKey}`}
                value={conflict.mine}
                theme={theme}
              />
              <ComparePane
                title={t("configEditor.conflict.theirs")}
                caption={t("configEditor.conflict.theirsCaption")}
                name={`config-conflict-stored-${conflictKey}`}
                value={conflict.theirs}
                theme={theme}
              />
            </div>
          )}
          <AlertDialogFooter className="sm:justify-between">
            <Button variant="ghost" onClick={() => setConflict(null)}>
              {t("common.back")}
            </Button>
            <div className="flex flex-col-reverse space-y-2 space-y-reverse sm:flex-row sm:space-x-2 sm:space-y-0">
              <Button variant="outline" onClick={resolveWithStored}>
                {t("configEditor.conflict.useStored")}
              </Button>
              <Button onClick={() => void resolveWithMine()}>
                {t("configEditor.conflict.useMine")}
              </Button>
            </div>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}

function ComparePane({
  title,
  caption,
  name,
  value,
  theme,
}: {
  title: string;
  caption: string;
  name: string;
  value: string;
  theme: "light" | "dark";
}) {
  return (
    <div className="min-w-0">
      <div className="flex items-baseline justify-between rounded-t-md border border-b-0 bg-muted/40 px-3 py-2">
        <span className="text-sm font-medium">{title}</span>
        <span className="text-xs text-muted-foreground">{caption}</span>
      </div>
      <div className="h-[min(52vh,460px)] rounded-b-md border bg-background">
        <JsonEditor
          name={name}
          defaultValue={value}
          theme={theme}
          readOnly
          maskedValuePaths={MASKED_PATHS}
        />
      </div>
    </div>
  );
}

function ToolButton({
  label,
  onClick,
  disabled,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  children: React.ReactNode;
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button variant="ghost" size="icon" className="h-8 w-8" onClick={onClick} disabled={disabled} />
        }
      >
        {children}
        <span className="sr-only">{label}</span>
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}
