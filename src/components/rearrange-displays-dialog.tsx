import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { commands, events, type DisplayLayout } from "@/lib/bindings";
import { fit, moveTile, placements, sameArrangement, toTiles, type Tile } from "@/lib/arrange";
import { cn } from "@/lib/utils";

const CANVAS_HEIGHT = 320;
const PADDING = 24;
// on-screen distance within which a dropped edge lines up with its neighbour's
const ALIGN_PX = 10;

type Drag = {
  key: string;
  pointerId: number;
  startX: number;
  startY: number;
  tileX: number;
  tileY: number;
  // the canvas transform frozen at the start, so nothing rescales under the pointer
  scale: number;
  offsetX: number;
  offsetY: number;
  x: number;
  y: number;
};

const tilesOf = (layout: DisplayLayout) =>
  toTiles(layout.displays.map(d => ({ ...d, mirrorGroup: d.mirrorGroup ?? null })));

const sameDisplays = (a: Tile[], b: Tile[]) =>
  a.length === b.length && a.every(t => b.some(o => o.key === t.key && o.ids.length === t.ids.length));

export function RearrangeDisplaysDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation();
  const [layout, setLayout] = useState<DisplayLayout | null>(null);
  const [original, setOriginal] = useState<Tile[]>([]);
  const [tiles, setTiles] = useState<Tile[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [applying, setApplying] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [width, setWidth] = useState(560);
  const canvasRef = useRef<HTMLDivElement | null>(null);
  // the handlers below read the latest values without re-subscribing on every move
  const tilesRef = useRef(tiles);
  const originalRef = useRef(original);
  tilesRef.current = tiles;
  originalRef.current = original;

  const dirty = !sameArrangement(original, tiles);

  const take = useCallback((next: DisplayLayout) => {
    const fresh = tilesOf(next);
    setLayout(next);
    setOriginal(fresh);
    setTiles(fresh);
  }, []);

  const load = useCallback(async () => {
    setLoadError(null);
    setNotice(null);
    try {
      const result = await commands.getDisplayLayout();
      if (result.status === "ok") take(result.data);
      else setLoadError(result.error);
    } catch (e) {
      console.error("failed to read the display layout", e);
      setLoadError(String(e));
    }
  }, [take]);

  useEffect(() => {
    if (!open) return;
    void load();
  }, [open, load]);

  // another program, or a display coming or going, changed the desktop while this was open
  useEffect(() => {
    if (!open) return;
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void events.displayLayoutChanged
      .listen(event => {
        const next = event.payload;
        const fresh = tilesOf(next);
        const before = originalRef.current;
        const edited = tilesRef.current;
        setLayout(next);
        if (sameDisplays(before, fresh)) {
          // same displays: keep the user's pending moves, measured against the new positions
          const pending = !sameArrangement(before, edited);
          setOriginal(fresh);
          if (!pending) setTiles(fresh);
          return;
        }
        const movedKeys = edited
          .filter(tile => {
            const was = before.find(o => o.key === tile.key);
            return was && (was.x !== tile.x || was.y !== tile.y);
          })
          .map(tile => tile.key);
        const allStillThere = movedKeys.every(key => fresh.some(f => f.key === key));
        setOriginal(fresh);
        if (movedKeys.length > 0 && allStillThere) {
          let kept = fresh;
          for (const key of movedKeys) {
            const tile = edited.find(e => e.key === key);
            if (tile) kept = moveTile(kept, key, tile.x, tile.y);
          }
          setTiles(kept);
          setNotice(t("rearrangeDisplays.displaysChanged"));
        } else {
          setTiles(fresh);
          setNotice(movedKeys.length > 0 ? t("rearrangeDisplays.changesLost") : t("rearrangeDisplays.displaysChanged"));
        }
      })
      .then(fn => {
        if (cancelled) fn();
        else unlisten = fn;
      });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [open, t]);

  useLayoutEffect(() => {
    if (!open) return;
    const measure = () => {
      const el = canvasRef.current;
      if (el) setWidth(el.clientWidth);
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, [open, layout]);

  const view = fit(tiles, width, CANVAS_HEIGHT, PADDING);
  const transform = drag ? { scale: drag.scale, offsetX: drag.offsetX, offsetY: drag.offsetY } : view;

  const close = () => {
    setDrag(null);
    setConfirmDiscard(false);
    onOpenChange(false);
  };

  const requestClose = () => {
    if (applying) return;
    if (dirty) setConfirmDiscard(true);
    else close();
  };

  const onPointerDown = (tile: Tile, e: ReactPointerEvent<HTMLDivElement>) => {
    if (tile.primary || applying || !layout?.canApply) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    setDrag({
      key: tile.key,
      pointerId: e.pointerId,
      startX: e.clientX,
      startY: e.clientY,
      tileX: tile.x,
      tileY: tile.y,
      scale: view.scale,
      offsetX: view.offsetX,
      offsetY: view.offsetY,
      x: tile.x,
      y: tile.y,
    });
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!drag || e.pointerId !== drag.pointerId) return;
    setDrag({
      ...drag,
      x: drag.tileX + (e.clientX - drag.startX) / drag.scale,
      y: drag.tileY + (e.clientY - drag.startY) / drag.scale,
    });
  };

  const onPointerUp = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!drag || e.pointerId !== drag.pointerId) return;
    const moved = drag.x !== drag.tileX || drag.y !== drag.tileY;
    if (moved) setTiles(moveTile(tiles, drag.key, drag.x, drag.y, ALIGN_PX / drag.scale));
    setDrag(null);
  };

  const apply = async () => {
    const changes = placements(original, tiles);
    if (changes.length === 0) {
      close();
      return;
    }
    setApplying(true);
    try {
      const result = await commands.applyDisplayLayout(changes);
      if (result.status === "ok") {
        take(result.data);
        close();
      } else {
        toast({
          variant: "destructive",
          title: t("toasts.rearrangeDisplays.failureTitle"),
          description: result.error || t("toasts.rearrangeDisplays.failureDescription"),
        });
      }
    } catch (e) {
      console.error("failed to apply the display layout", e);
      toast({
        variant: "destructive",
        title: t("toasts.rearrangeDisplays.failureTitle"),
        description: t("toasts.rearrangeDisplays.failureDescription"),
      });
    } finally {
      setApplying(false);
    }
  };

  const blockedReason = layout && !layout.canApply ? layout.reason ?? t("rearrangeDisplays.unavailable") : null;

  return (
    <>
      <Dialog
        open={open}
        onOpenChange={next => {
          if (next) onOpenChange(true);
          else requestClose();
        }}
      >
        <DialogContent
          className="max-w-2xl"
          onInteractOutside={e => {
            e.preventDefault();
            requestClose();
          }}
          onEscapeKeyDown={e => {
            e.preventDefault();
            requestClose();
          }}
        >
          <DialogHeader>
            <DialogTitle>{t("rearrangeDisplays.title")}</DialogTitle>
            <DialogDescription>{t("rearrangeDisplays.description")}</DialogDescription>
          </DialogHeader>

          <div
            ref={canvasRef}
            className="relative mt-4 w-full overflow-hidden rounded-md border bg-muted/40 select-none"
            style={{ height: CANVAS_HEIGHT, touchAction: "none" }}
          >
            {loadError && (
              <p className="absolute left-0 right-0 top-1/2 px-6 text-center text-sm text-red-500">
                {t("rearrangeDisplays.loadFailed", { error: loadError })}
              </p>
            )}
            {!loadError && !layout && (
              <p className="absolute left-0 right-0 top-1/2 text-center text-sm text-muted-foreground">
                {t("rearrangeDisplays.loading")}
              </p>
            )}
            {!loadError && layout && tiles.length === 0 && (
              <p className="absolute left-0 right-0 top-1/2 px-6 text-center text-sm text-muted-foreground">
                {blockedReason ?? t("rearrangeDisplays.none")}
              </p>
            )}
            {tiles.map(tile => {
              const dragging = drag?.key === tile.key;
              const x = dragging && drag ? drag.x : tile.x;
              const y = dragging && drag ? drag.y : tile.y;
              const movable = !tile.primary && !!layout?.canApply && !applying;
              return (
                <div
                  key={tile.key}
                  onPointerDown={e => onPointerDown(tile, e)}
                  onPointerMove={onPointerMove}
                  onPointerUp={onPointerUp}
                  onPointerCancel={() => setDrag(null)}
                  className={cn(
                    "absolute flex flex-col items-center justify-center overflow-hidden rounded-sm border-2 px-1 text-center",
                    tile.ours ? "border-blue-500 bg-blue-500/15" : "border-foreground/40 bg-background",
                    tile.primary && "border-foreground/80",
                    movable ? "cursor-grab" : "cursor-default",
                    dragging && "z-10 cursor-grabbing opacity-80 shadow-lg",
                  )}
                  style={{
                    left: x * transform.scale + transform.offsetX,
                    top: y * transform.scale + transform.offsetY,
                    width: Math.max(1, tile.width * transform.scale - 2),
                    height: Math.max(1, tile.height * transform.scale - 2),
                  }}
                  title={tile.names.join(", ")}
                >
                  <span className="max-w-full truncate text-xs font-medium">{tile.names.join(" + ")}</span>
                  <span className="max-w-full truncate text-[10px] text-muted-foreground">
                    {tile.primary
                      ? t("rearrangeDisplays.primary")
                      : tile.ours
                        ? t("rearrangeDisplays.screenExtendDisplay")
                        : `${tile.width} × ${tile.height}`}
                  </span>
                </div>
              );
            })}
          </div>

          {notice && <p className="mt-3 text-sm text-amber-600">{notice}</p>}
          {blockedReason && tiles.length > 0 && <p className="mt-3 text-sm text-muted-foreground">{blockedReason}</p>}

          <DialogFooter className="mt-4">
            <Button variant="outline" onClick={requestClose} disabled={applying}>
              {t("common.cancel")}
            </Button>
            <Button onClick={() => void apply()} disabled={applying || !layout?.canApply}>
              {applying ? t("rearrangeDisplays.applying") : t("rearrangeDisplays.done")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <AlertDialog open={confirmDiscard} onOpenChange={setConfirmDiscard}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("rearrangeDisplays.discardTitle")}</AlertDialogTitle>
            <AlertDialogDescription>{t("rearrangeDisplays.discardDescription")}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>{t("rearrangeDisplays.keepEditing")}</AlertDialogCancel>
            <AlertDialogAction className="bg-red-600 hover:bg-red-700 text-white" onClick={close}>
              {t("rearrangeDisplays.discard")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}
