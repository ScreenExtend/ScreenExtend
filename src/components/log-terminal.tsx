import { useEffect, useRef, useState } from "react";

import { commands, events } from "@/lib/bindings";

const MAX_LINES = 2000;

export function LogTerminal() {
  const [lines, setLines] = useState<string[]>([]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    void (async () => {
      const backlog = await commands.getLogBacklog();
      if (!active) return;
      setLines(backlog);
      unlisten = await events.logLine.listen(event => {
        setLines(prev => {
          const next = [...prev, event.payload];
          return next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
        });
      });
    })();
    return () => {
      active = false;
      if (unlisten) unlisten();
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
