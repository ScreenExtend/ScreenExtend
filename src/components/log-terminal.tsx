import { useEffect, useRef, useState } from "react";

import { useToast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { commands, events } from "@/lib/bindings";

const MAX_LINES = 2000;

const stopListening = (unlisten: () => void) => {
  void Promise.resolve(unlisten()).catch(e => console.error("log unlisten failed", e));
};

export function LogTerminal() {
  const [lines, setLines] = useState<string[]>([]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const { toast } = useToast();
  const { t } = useTranslation();

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    void (async () => {
      let backlogFailed = false;
      try {
        const backlog = await commands.getLogBacklog();
        if (!active) return;
        setLines(backlog);
      } catch (e) {
        console.error("log backlog read failed", e);
        backlogFailed = true;
      }
      if (!active) return;
      try {
        const off = await events.logLine.listen(event => {
          setLines(prev => {
            const next = [...prev, event.payload];
            return next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
          });
        });
        if (!active) {
          stopListening(off);
          return;
        }
        unlisten = off;
        if (backlogFailed) {
          toast({
            variant: "destructive",
            title: t("toasts.logs.backlogFailedTitle"),
            description: t("toasts.logs.backlogFailedDescription"),
          });
        }
      } catch (e) {
        console.error("log stream subscribe failed", e);
        if (!active) return;
        toast({
          variant: "destructive",
          title: t("toasts.logs.streamFailedTitle"),
          description: t("toasts.logs.streamFailedDescription"),
        });
      }
    })();
    return () => {
      active = false;
      if (unlisten) stopListening(unlisten);
    };
  }, []);

  useEffect(() => {
    const el = scrollRef.current;
    if (el && pinnedRef.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [lines]);

  const handleScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    pinnedRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
  };

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-end">
        <button
          type="button"
          className="text-xs text-muted-foreground hover:text-foreground transition-colors"
          onClick={() => setLines([])}
        >
          Clear
        </button>
      </div>
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="h-72 w-full overflow-y-auto rounded-md border bg-zinc-950 p-3 font-mono text-xs leading-relaxed text-zinc-200"
      >
        {lines.length === 0 ? (
          <div className="text-zinc-500">Waiting for logs…</div>
        ) : (
          lines.map((line, i) => (
            <div key={i} className="whitespace-pre-wrap break-all">
              {line || " "}
            </div>
          ))
        )}
      </div>
    </div>
  );
}
