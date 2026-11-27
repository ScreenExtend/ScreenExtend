#!/usr/bin/env node
// The Rearrange Displays geometry: `pnpm test:arrange`.
//
// No runner and no devDependency. Node strips the types out of `src/lib/arrange.ts` on import,
// so this runs the module the dialog ships.
//
// What it pins: a dropped display always ends up touching the others and overlapping none, the
// primary never moves, a mirror group moves as one, and only what moved is sent to the host.
import assert from 'node:assert/strict';
import {
  fit,
  isConnected,
  moveTile,
  normalize,
  placements,
  sameArrangement,
  snap,
  toTiles,
  touches,
} from '../src/lib/arrange.ts';

let failed = 0;
function test(what, run) {
  try {
    run();
    console.log('  ok   ' + what);
  } catch (e) {
    failed += 1;
    console.log('  FAIL ' + what + '\n       ' + (e && e.message));
  }
}

const display = (id, x, y, width, height, extra = {}) => ({
  id, name: id, x, y, width, height, primary: false, ours: false, mirrorGroup: null, ...extra,
});
const overlapping = (tiles) => tiles.some((a, i) => tiles.some((b, j) =>
  i !== j && a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height));

// a 2560x1440 primary with a 1920x1080 virtual display to its right
const base = () => toTiles([
  display('main', 0, 0, 2560, 1440, { primary: true }),
  display('virt', 2560, 0, 1920, 1080, { ours: true }),
]);

test('a display dropped in empty space snaps to the nearest edge', () => {
  const tiles = moveTile(base(), 'virt', 300, -2000);
  const virt = tiles.find(t => t.key === 'virt');
  assert.equal(virt.y, -1080, 'lands on top of the primary');
  assert.ok(touches(virt, tiles[0]));
  assert.ok(!overlapping(tiles));
});

test('a display dropped on top of another is pushed out, never overlapping', () => {
  const tiles = moveTile(base(), 'virt', 100, 100);
  assert.ok(!overlapping(tiles));
  assert.ok(isConnected(tiles));
});

test('the primary does not move', () => {
  const tiles = moveTile(base(), 'main', 500, 500);
  assert.deepEqual(tiles.map(t => [t.x, t.y]), [[0, 0], [2560, 0]]);
});

test('edges within the alignment distance line up', () => {
  const others = [base()[0]];
  const virt = base()[1];
  assert.equal(snap(virt, 2560, 20, others, 40).y, 0, 'tops line up');
  assert.equal(snap(virt, 2560, 20, others, 0).y, 20, 'no alignment asked: stays at 20');
  assert.equal(snap(virt, 2560, 380, others, 40).y, 360, 'bottoms line up (1440 - 1080)');
});

test('moving the middle of a chain reattaches what it left floating', () => {
  const tiles = toTiles([
    display('a', 0, 0, 1920, 1080, { primary: true }),
    display('b', 1920, 0, 1920, 1080),
    display('c', 3840, 0, 1920, 1080),
  ]);
  const moved = moveTile(tiles, 'b', 0, -5000);
  assert.ok(isConnected(moved), 'still one connected desktop');
  assert.ok(!overlapping(moved));
  assert.deepEqual([moved[0].x, moved[0].y], [0, 0]);
});

test('a mirror group is one tile and sends a placement for every member', () => {
  const tiles = toTiles([
    display('main', 0, 0, 1920, 1080, { primary: true }),
    display('m1', 1920, 0, 1920, 1080, { mirrorGroup: 7 }),
    display('m2', 1920, 0, 1920, 1080, { mirrorGroup: 7 }),
  ]);
  assert.equal(tiles.length, 2);
  const moved = moveTile(tiles, 'm1', -1920, 0);
  const sent = placements(tiles, moved);
  assert.deepEqual(sent.map(p => p.id).sort(), ['m1', 'm2']);
  assert.ok(sent.every(p => p.x === -1920 && p.y === 0));
});

test('only displays that moved are sent, and nothing when nothing moved', () => {
  const tiles = base();
  assert.deepEqual(placements(tiles, tiles), []);
  assert.ok(sameArrangement(tiles, base()));
  const moved = moveTile(tiles, 'virt', -1920, 0);
  assert.deepEqual(placements(tiles, moved), [{ id: 'virt', x: -1920, y: 0 }]);
  assert.ok(!sameArrangement(tiles, moved));
});

test('normalize puts the primary back at the origin', () => {
  const shifted = base().map(t => ({ ...t, x: t.x + 100, y: t.y - 50 }));
  const n = normalize(shifted);
  assert.deepEqual([n[0].x, n[0].y, n[1].x, n[1].y], [0, 0, 2560, 0]);
});

test('fit keeps every display inside the canvas', () => {
  const tiles = base();
  const { scale, offsetX, offsetY } = fit(tiles, 600, 300, 20);
  for (const t of tiles) {
    assert.ok(t.x * scale + offsetX >= 19.5 && (t.x + t.width) * scale + offsetX <= 580.5);
    assert.ok(t.y * scale + offsetY >= 19.5 && (t.y + t.height) * scale + offsetY <= 280.5);
  }
});

console.log(failed === 0 ? '\nall arrange assertions passed' : `\n${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
