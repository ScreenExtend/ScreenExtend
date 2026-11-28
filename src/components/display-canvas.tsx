import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import { useBlocker } from "react-router-dom";

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
import { DeviceDetails } from "@/components/pages/device-details";
import { saveDevicePlacements, type ConnectedDevice } from "@/components/config-provider";
import { useTranslation } from "@/i18n";
import { commands, events, type DisplayLayout } from "@/lib/bindings";
import { devicePlacements, fit, moveTile, placements, sameArrangement, savedPlacements, toTiles, type Tile } from "@/lib/arrange";
import { cn } from "@/lib/utils";
import { formatHz } from "@/lib/format";

const CANVAS_HEIGHT = 420;
const PADDING = 32;
// on-screen distance within which a dropped edge lines up with its neighbour's
const ALIGN_PX = 10;
// pointer travel below this is a click, not a drag
const CLICK_SLOP_PX = 4;
// tile text, scaled with the tile between these sizes
const TITLE_MIN_PX = 12;
const TITLE_MAX_PX = 18;

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
  // past the click slop: from here on it is a move
  moved: boolean;
  movable: boolean;
};

const tilesOf = (layout: DisplayLayout) =>
  toTiles(layout.displays.map(d => ({ ...d, mirrorGroup: d.mirrorGroup ?? null })));

const sameDisplays = (a: Tile[], b: Tile[]) =>
  a.length === b.length && a.every(t => b.some(o => o.key === t.key && o.ids.length === t.ids.length));

const size = (tile: Tile) => `${tile.width} × ${tile.height}`;

/// The desktop as the OS arranges it, drawn to scale. Displays are dragged in place and the
/// moves are applied or discarded from the bar under it.
export function DisplayCanvas({
  devices,
  labelFor,
}: {
  devices: ConnectedDevice[];
  labelFor: (device: ConnectedDevice) => string;
}) {
  const { t } = useTranslation();
  const [layout, setLayout] = useState<DisplayLayout | null>(null);
  const [original, setOriginal] = useState<Tile[]>([]);
  const [tiles, setTiles] = useState<Tile[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [applying, setApplying] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [width, setWidth] = useState(720);
  // per join, bumped to open that device's panel
  const [panelRequests, setPanelRequests] = useState<Record<string, number>>({});
  const canvasRef = useRef<HTMLDivElement | null>(null);
  // the handlers below read the latest values without re-subscribing on every move
  const tilesRef = useRef(tiles);
  const originalRef = useRef(original);
  tilesRef.current = tiles;
  originalRef.current = original;

  const dirty = !sameArrangement(original, tiles);

  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) => dirty && currentLocation.pathname !== nextLocation.pathname,
  );

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
    void load();
  }, [load]);

  // a device joining gives its display a session to link to only after the layout was read
  const joinIds = devices.map(d => d.joinId).sort().join(",");
  useEffect(() => {
    if (!dirty) void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [joinIds]);

  // another program, or a display coming or going, changed the desktop
  useEffect(() => {
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
          setNotice(movedKeys.length > 0 ? t("rearrangeDisplays.changesLost") : null);
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
  }, [t]);

  useLayoutEffect(() => {
    const measure = () => {
      const el = canvasRef.current;
      if (el) setWidth(el.clientWidth);
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, [layout]);

  const view = fit(tiles, width, CANVAS_HEIGHT, PADDING);
  const transform = drag ? { scale: drag.scale, offsetX: drag.offsetX, offsetY: drag.offsetY } : view;

  const discard = () => {
    setDrag(null);
    setTiles(original);
    setNotice(null);
  };

  const deviceOf = (tile: Tile) =>
    tile.ours && tile.joinId ? devices.find(d => d.joinId === tile.joinId) : undefined;

  const openPanel = (joinId: string) =>
    setPanelRequests(prev => ({ ...prev, [joinId]: (prev[joinId] ?? 0) + 1 }));

  const onPointerDown = (tile: Tile, e: ReactPointerEvent<HTMLDivElement>) => {
    const movable = !applying && !!layout?.canApply;
    if (!movable && !deviceOf(tile)) return;
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
      moved: false,
      movable,
    });
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!drag || e.pointerId !== drag.pointerId || !drag.movable) return;
    const dx = e.clientX - drag.startX;
    const dy = e.clientY - drag.startY;
    if (!drag.moved && Math.abs(dx) < CLICK_SLOP_PX && Math.abs(dy) < CLICK_SLOP_PX) return;
    setDrag({
      ...drag,
      moved: true,
      x: drag.tileX + dx / drag.scale,
      y: drag.tileY + dy / drag.scale,
    });
  };

  const onPointerUp = (tile: Tile, e: ReactPointerEvent<HTMLDivElement>) => {
    if (!drag || e.pointerId !== drag.pointerId) return;
    if (drag.moved) {
      setTiles(moveTile(tiles, drag.key, drag.x, drag.y, ALIGN_PX / drag.scale));
    } else {
      const device = deviceOf(tile);
      if (device) openPanel(device.joinId);
    }
    setDrag(null);
  };

  // each device's display goes back where it was placed the next time it joins
  const rememberPlacements = async (applied: DisplayLayout) => {
    try {
      const byJoin = new Map(devices.map(d => [d.joinId, d]));
      const placed = devicePlacements(applied.displays, j => byJoin.get(j)?.token)
        .map(p => ({ device: devices.find(d => d.token === p.token)!, x: p.x, y: p.y }))
        .filter(p => !!p.device);
      if (placed.length === 0) return;
      const rows = await saveDevicePlacements(placed);
      await commands.setDevicePlacements(savedPlacements(rows));
    } catch (e) {
      console.error("could not save where the displays were placed", e);
    }
  };

  const apply = async () => {
    const changes = placements(original, tiles);
    if (changes.length === 0) return;
    setApplying(true);
    try {
      const result = await commands.applyDisplayLayout(changes);
      if (result.status === "ok") {
        take(result.data);
        setNotice(null);
        await rememberPlacements(result.data);
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

  const renderTile = (tile: Tile) => {
    const dragging = drag?.key === tile.key && drag.moved;
    const x = dragging && drag ? drag.x : tile.x;
    const y = dragging && drag ? drag.y : tile.y;
    const w = Math.max(1, tile.width * transform.scale - 2);
    const h = Math.max(1, tile.height * transform.scale - 2);
    const movable = !!layout?.canApply && !applying;
    const device = deviceOf(tile);
    const lines: string[] = device
      ? [labelFor(device), device.os, size(tile), formatHz(device.refreshRate)].filter(Boolean)
      : [
        tile.names.join(" + "),
        tile.primary
          ? t("rearrangeDisplays.primary")
          : tile.ours
            ? t("rearrangeDisplays.screenExtendDisplay")
            : "",
        size(tile),
        formatHz(tile.refreshHz),
      ].filter(Boolean);
    // text follows the tile's size, so a big display does not hold a tiny label
    const title = Math.round(Math.min(TITLE_MAX_PX, Math.max(TITLE_MIN_PX, Math.min(h / 7, w / 8))));
    const detail = Math.max(10, Math.round(title * 0.8));
    const lineHeight = (px: number) => Math.round(px * 1.3);
    // as many lines as fit, the most important first
    let used = lineHeight(title);
    let room = 1;
    while (room < lines.length && used + lineHeight(detail) <= h - 8) {
      used += lineHeight(detail);
      room += 1;
    }
    return (
      <div
        key={tile.key}
        onPointerDown={e => onPointerDown(tile, e)}
        onPointerMove={onPointerMove}
        onPointerUp={e => onPointerUp(tile, e)}
        onPointerCancel={() => setDrag(null)}
        className={cn(
          "absolute flex flex-col items-center justify-center overflow-hidden rounded-sm border-2 px-2 text-center",
          tile.ours ? "border-blue-500 bg-blue-500/15" : "border-foreground/40 bg-background",
          tile.primary && "border-foreground/80",
          device && "hover:bg-blue-500/25",
          movable ? "cursor-grab" : device ? "cursor-pointer" : "cursor-default",
          dragging && "z-10 cursor-grabbing opacity-80 shadow-lg",
        )}
        style={{
          left: x * transform.scale + transform.offsetX,
          top: y * transform.scale + transform.offsetY,
          width: w,
          height: h,
        }}
        title={(device ? [...lines, t("rearrangeDisplays.clickToEdit")] : lines).join("\n")}
      >
        {lines.slice(0, room).map((line, i) => (
          <span
            key={i}
            className={cn("block max-w-full truncate", i === 0 ? "font-semibold" : "text-muted-foreground")}
            style={{
              fontSize: i === 0 ? title : detail,
              lineHeight: `${lineHeight(i === 0 ? title : detail)}px`,
            }}
          >
            {line}
          </span>
        ))}
      </div>
    );
  };

  return (
    <div>
      <div
        ref={canvasRef}
        className="relative w-full overflow-hidden rounded-md border bg-muted/40 select-none"
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
        {tiles.map(renderTile)}
      </div>

      {devices.map(device => (
        <DeviceDetails
          key={device.joinId}
          device={device}
          trigger={null}
          openRequest={panelRequests[device.joinId] ?? 0}
        />
      ))}

      {notice && <p className="mt-3 text-sm text-amber-600">{notice}</p>}
      {blockedReason && tiles.length > 0 && <p className="mt-3 text-sm text-muted-foreground">{blockedReason}</p>}

      {dirty && (
        <div className="mt-3 flex items-center justify-end rounded-md border bg-background px-3 py-2">
          <span className="mr-auto text-sm text-muted-foreground">{t("rearrangeDisplays.pending")}</span>
          <Button variant="outline" size="sm" className="mr-2" onClick={discard} disabled={applying}>
            {t("rearrangeDisplays.discard")}
          </Button>
          <Button size="sm" onClick={() => void apply()} disabled={applying || !layout?.canApply}>
            {applying ? t("rearrangeDisplays.applying") : t("rearrangeDisplays.apply")}
          </Button>
        </div>
      )}

      <AlertDialog
        open={blocker.state === "blocked"}
        onOpenChange={next => {
          if (!next) blocker.reset?.();
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("rearrangeDisplays.discardTitle")}</AlertDialogTitle>
            <AlertDialogDescription>{t("rearrangeDisplays.discardDescription")}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={() => blocker.reset?.()}>{t("rearrangeDisplays.keepEditing")}</AlertDialogCancel>
            <AlertDialogAction
              className="bg-red-600 hover:bg-red-700 text-white"
              onClick={() => {
                discard();
                blocker.proceed?.();
              }}
            >
              {t("rearrangeDisplays.discard")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
