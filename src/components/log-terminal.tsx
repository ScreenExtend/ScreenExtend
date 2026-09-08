import { useEffect, useRef, useState } from "react";

import { toast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { commands, events } from "@/lib/bindings";

const MAX_LINES = 2000;

// A monotonic sequence number as the React key. Keyed by array index, every row's text
// changed once the 2000-line window started sliding, so React re-painted the whole list
// on every event instead of appending one node.
type LogEntry = { seq: number; text: string };

const stopListening = (unlisten: () => void) => {
  void Promise.resolve(unlisten()).catch(e => console.error("log unlisten failed", e));
};

export function LogTerminal() {
  const [lines, setLines] = useState<LogEntry[]>([]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const pending = useRef<LogEntry[]>([]);
  const flushHandle = useRef<number | undefined>(undefined);
  const nextSeq = useRef(0);
  const { t } = useTranslation();

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    const flush = () => {
      flushHandle.current = undefined;
      const batch = pending.current;
      if (!batch.length) return;
      pending.current = [];
      setLines(prev => {
        const next = prev.concat(batch);
        return next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
      });
    };
    void (async () => {
      let backlogFailed = false;
      try {
        const backlog = await commands.getLogBacklog();
        if (!active) return;
        setLines(backlog.map(text => ({ seq: nextSeq.current++, text })));
      } catch (e) {
        console.error("log backlog read failed", e);
        backlogFailed = true;
      }
      if (!active) return;
      try {
        // A device join, an encoder negotiation or a cloud reconnect storm arrives as a
        // burst; coalescing a frame's worth into one commit keeps that off the main thread.
        const off = await events.logLine.listen(event => {
          pending.current.push({ seq: nextSeq.current++, text: event.payload });
          if (flushHandle.current === undefined) {
            flushHandle.current = window.requestAnimationFrame(flush);
          }
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
      if (flushHandle.current !== undefined) window.cancelAnimationFrame(flushHandle.current);
      flushHandle.current = undefined;
      pending.current = [];
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
          onClick={() => { pending.current = []; setLines([]); }}
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
          lines.map(line => (
            <div key={line.seq} className="whitespace-pre-wrap break-all">
              {line.text || " "}
            </div>
          ))
        )}
      </div>
    </div>
  );
}
