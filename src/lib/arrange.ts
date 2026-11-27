// Geometry for the Rearrange Displays dialog, kept free of React and Tauri so
// `scripts/test-arrange.mjs` can run it under plain node.
//
// Coordinates are the layout's own units (pixels on Windows, points on macOS), y pointing down,
// the primary display at (0, 0).

export type Tile = {
  /// The first member's id; a mirror group moves as one tile.
  key: string;
  ids: string[];
  names: string[];
  x: number;
  y: number;
  width: number;
  height: number;
  primary: boolean;
  ours: boolean;
};

export type LayoutDisplay = {
  id: string;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  primary: boolean;
  ours: boolean;
  mirrorGroup: number | null;
};

export type Placement = { id: string; x: number; y: number };

/// One tile per display, except that displays in one mirror group share a tile.
export function toTiles(displays: readonly LayoutDisplay[]): Tile[] {
  const tiles: Tile[] = [];
  const byGroup = new Map<number, Tile>();
  for (const d of displays) {
    const group = d.mirrorGroup;
    const existing = group === null || group === undefined ? undefined : byGroup.get(group);
    if (existing) {
      existing.ids.push(d.id);
      existing.names.push(d.name);
      existing.primary = existing.primary || d.primary;
      existing.ours = existing.ours || d.ours;
      continue;
    }
    const tile: Tile = {
      key: d.id,
      ids: [d.id],
      names: [d.name],
      x: d.x,
      y: d.y,
      width: d.width,
      height: d.height,
      primary: d.primary,
      ours: d.ours,
    };
    tiles.push(tile);
    if (group !== null && group !== undefined) byGroup.set(group, tile);
  }
  return tiles;
}

const overlaps = (a: Tile, b: Tile) =>
  a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;

/// Sharing a stretch of edge, not just a corner: that is what both systems call touching.
export function touches(a: Tile, b: Tile): boolean {
  const sideBySide =
    (a.x + a.width === b.x || b.x + b.width === a.x) && a.y < b.y + b.height && b.y < a.y + a.height;
  const stacked =
    (a.y + a.height === b.y || b.y + b.height === a.y) && a.x < b.x + b.width && b.x < a.x + a.width;
  return sideBySide || stacked;
}

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/// Where `moving` lands when dropped at (x, y): the nearest spot that touches one of `others`
/// and overlaps none of them. Edges within `align` units of an edge it could line up with are
/// lined up, as the system panels do.
export function snap(moving: Tile, x: number, y: number, others: readonly Tile[], align = 0): { x: number; y: number } {
  if (others.length === 0) return { x: Math.round(x), y: Math.round(y) };
  const w = moving.width;
  const h = moving.height;
  const candidates: { x: number; y: number }[] = [];
  const alignTo = (v: number, targets: number[]) => {
    let best = v;
    let bestDistance = align + 1;
    for (const t of targets) {
      const distance = Math.abs(v - t);
      if (distance <= align && distance < bestDistance) {
        best = t;
        bestDistance = distance;
      }
    }
    return best;
  };
  for (const o of others) {
    // beside it: any vertical position that keeps at least one unit of shared edge
    const ySide = alignTo(clamp(Math.round(y), o.y - h + 1, o.y + o.height - 1), [o.y, o.y + o.height - h]);
    candidates.push({ x: o.x - w, y: ySide }, { x: o.x + o.width, y: ySide });
    // above or below it
    const xStack = alignTo(clamp(Math.round(x), o.x - w + 1, o.x + o.width - 1), [o.x, o.x + o.width - w]);
    candidates.push({ x: xStack, y: o.y - h }, { x: xStack, y: o.y + o.height });
  }
  let best: { x: number; y: number } | null = null;
  let bestDistance = Infinity;
  for (const c of candidates) {
    const placed = { ...moving, x: c.x, y: c.y };
    if (others.some(o => overlaps(placed, o))) continue;
    const distance = (c.x - x) * (c.x - x) + (c.y - y) * (c.y - y);
    if (distance < bestDistance) {
      best = c;
      bestDistance = distance;
    }
  }
  // every side of every display is blocked only in contrived layouts: leave it where it was
  return best ?? { x: moving.x, y: moving.y };
}

/// Whether every tile can be reached from every other through shared edges.
export function isConnected(tiles: readonly Tile[]): boolean {
  if (tiles.length <= 1) return true;
  const seen = new Set<number>([0]);
  const queue = [0];
  while (queue.length > 0) {
    const i = queue.shift() as number;
    tiles.forEach((t, j) => {
      if (!seen.has(j) && touches(tiles[i], t)) {
        seen.add(j);
        queue.push(j);
      }
    });
  }
  return seen.size === tiles.length;
}

/// Puts the primary back at (0, 0) by moving everything by the same amount.
export function normalize(tiles: readonly Tile[]): Tile[] {
  const primary = tiles.find(t => t.primary);
  if (!primary || (primary.x === 0 && primary.y === 0)) return tiles.map(t => ({ ...t }));
  const dx = primary.x;
  const dy = primary.y;
  return tiles.map(t => ({ ...t, x: t.x - dx, y: t.y - dy }));
}

/// Moves one tile, snaps it, then reattaches anything the move left floating, so the result is
/// always a layout the system accepts. The primary never moves.
export function moveTile(tiles: readonly Tile[], key: string, x: number, y: number, align = 0): Tile[] {
  const next = tiles.map(t => ({ ...t }));
  const moving = next.find(t => t.key === key);
  if (!moving || moving.primary) return next;
  const others = next.filter(t => t !== moving);
  const spot = snap(moving, x, y, others, align);
  moving.x = spot.x;
  moving.y = spot.y;
  return reattach(next, moving.key);
}

/// Snaps every tile cut off from the primary back onto the part that holds it, nearest first.
/// `fixed` is the tile the user just placed: it stays put.
export function reattach(tiles: Tile[], fixed?: string): Tile[] {
  for (let guard = 0; guard < tiles.length; guard += 1) {
    const anchor = tiles.findIndex(t => t.primary);
    const reached = new Set<number>([anchor < 0 ? 0 : anchor]);
    const queue = [...reached];
    while (queue.length > 0) {
      const i = queue.shift() as number;
      tiles.forEach((t, j) => {
        if (!reached.has(j) && touches(tiles[i], t)) {
          reached.add(j);
          queue.push(j);
        }
      });
    }
    if (reached.size === tiles.length) return tiles;
    const connected = tiles.filter((_, i) => reached.has(i));
    const loose = tiles.filter((t, i) => !reached.has(i) && t.key !== fixed);
    // the placed tile itself is cut off only when nothing else could hold it
    const pick = loose.length > 0 ? loose : tiles.filter((_, i) => !reached.has(i));
    let nearest = pick[0];
    let nearestDistance = Infinity;
    for (const t of pick) {
      for (const c of connected) {
        const d = Math.abs(t.x - c.x) + Math.abs(t.y - c.y);
        if (d < nearestDistance) {
          nearest = t;
          nearestDistance = d;
        }
      }
    }
    const spot = snap(nearest, nearest.x, nearest.y, connected);
    nearest.x = spot.x;
    nearest.y = spot.y;
  }
  return tiles;
}

/// What to send to the host: one placement per display whose tile moved, relative to the
/// primary. Every member of a mirror group moves with it.
export function placements(before: readonly Tile[], after: readonly Tile[]): Placement[] {
  const original = new Map(before.map(t => [t.key, t]));
  const out: Placement[] = [];
  for (const t of normalize(after)) {
    const was = original.get(t.key);
    if (was && was.x === t.x && was.y === t.y) continue;
    for (const id of t.ids) out.push({ id, x: t.x, y: t.y });
  }
  return out;
}

export function sameArrangement(a: readonly Tile[], b: readonly Tile[]): boolean {
  if (a.length !== b.length) return false;
  const byKey = new Map(b.map(t => [t.key, t]));
  return a.every(t => {
    const o = byKey.get(t.key);
    return !!o && o.x === t.x && o.y === t.y;
  });
}

/// Scale and offset that fit every tile into a `width` by `height` box with `padding` around.
export function fit(tiles: readonly Tile[], width: number, height: number, padding: number) {
  if (tiles.length === 0) return { scale: 1, offsetX: 0, offsetY: 0 };
  const minX = Math.min(...tiles.map(t => t.x));
  const minY = Math.min(...tiles.map(t => t.y));
  const maxX = Math.max(...tiles.map(t => t.x + t.width));
  const maxY = Math.max(...tiles.map(t => t.y + t.height));
  const spanX = Math.max(1, maxX - minX);
  const spanY = Math.max(1, maxY - minY);
  const scale = Math.min((width - 2 * padding) / spanX, (height - 2 * padding) / spanY);
  return {
    scale,
    offsetX: (width - spanX * scale) / 2 - minX * scale,
    offsetY: (height - spanY * scale) / 2 - minY * scale,
  };
}
