// Scans build output for JS/DOM APIs that macOS 10.15's WebKit (Safari 13.1)
// does not have. minimumSystemVersion is 10.15, nothing type-checks this for
// us, and a missing API inside a dependency is a hard crash rather than a
// degraded feature -- monaco's MediaQueryList call runs at module scope and
// took down the whole settings editor chunk.
//
//   pnpm build && node scripts/check-legacy-webkit.mjs [dir]
//
// `cover` says why a hit is survivable: "shim" is patched in
// src/lib/legacy-webview-polyfills.ts (keep the two in step), "unreachable"
// carries the reason. Anything else fails the run.
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const CHECKS = [
  { label: "WeakRef", re: /\bnew WeakRef\s*\(/g, since: "14.1", cover: "shim" },
  { label: "FinalizationRegistry", re: /\bFinalizationRegistry\b/g, since: "14.1" },
  { label: "Promise.any", re: /\bPromise\s*\.\s*any\s*\(/g, since: "14" },
  { label: "AggregateError", re: /\bnew AggregateError\s*\(/g, since: "14", cover: "shim" },
  { label: "new EventTarget()", re: /\bnew EventTarget\s*\(/g, since: "14" },
  { label: "replaceChildren", re: /\.replaceChildren\s*\(/g, since: "14", cover: "shim" },
  { label: "MediaQueryList events", re: /matchMedia\([^)]*\)\s*\.\s*(add|remove)EventListener/g, since: "14", cover: "shim" },
  {
    label: "Intl.Segmenter",
    re: /\bnew Intl\s*\.\s*Segmenter\b/g,
    since: "14.1",
    cover: "unreachable",
    why: "monaco only builds one when editor.wordSegmenterLocales is set; we never set it",
  },
  { label: "Intl.DisplayNames", re: /\bIntl\s*\.\s*DisplayNames\b/g, since: "14" },
  { label: "Intl.ListFormat", re: /\bIntl\s*\.\s*ListFormat\b/g, since: "14" },
  { label: "Error cause", re: /\bnew \w*Error\([^)]*\{\s*cause\s*:/g, since: "15" },
  { label: "Object.hasOwn", re: /\bObject\s*\.\s*hasOwn\s*\(/g, since: "15.4" },
  { label: "structuredClone", re: /\bstructuredClone\s*\(/g, since: "15.4" },
  { label: "relative indexing .at()", re: /\.at\s*\(/g, since: "15.4", cover: "shim" },
  { label: "findLast/findLastIndex", re: /\.findLast(?:Index)?\s*\(/g, since: "15.4", cover: "shim" },
  { label: "crypto.randomUUID", re: /\brandomUUID\s*\(/g, since: "15.4" },
  { label: "dialog.showModal", re: /\.showModal\s*\(/g, since: "15.4" },
  { label: "BroadcastChannel", re: /\bnew BroadcastChannel\b/g, since: "15.4" },
  { label: "AbortSignal.abort/timeout", re: /\bAbortSignal\s*\.\s*(abort|timeout)\s*\(/g, since: "15.4" },
  { label: "navigator.locks", re: /\bnavigator\s*\.\s*locks\b/g, since: "15.4" },
  { label: "reportError", re: /\breportError\s*\(/g, since: "15.4" },
  { label: "requestIdleCallback", re: /\brequestIdleCallback\s*\(/g, since: "15.4" },
  { label: "CSS.registerProperty", re: /\bCSS\s*\.\s*registerProperty\b/g, since: "16.4" },
  { label: "ElementInternals", re: /\.attachInternals\s*\(/g, since: "16.4" },
  { label: "TextDecoderStream", re: /\bnew Text(?:Decoder|Encoder)Stream\b/g, since: "16.4" },
  { label: "Array.fromAsync", re: /\bArray\s*\.\s*fromAsync\b/g, since: "16.4" },
  { label: "toSorted/toReversed", re: /\.(toSorted|toReversed|toSpliced)\s*\(/g, since: "16.4" },
  { label: "Object.groupBy", re: /\bObject\s*\.\s*groupBy\b/g, since: "17.4" },
  { label: "Promise.withResolvers", re: /\bwithResolvers\s*\(/g, since: "17.4" },
  { label: "URL.canParse", re: /\bURL\s*\.\s*canParse\b/g, since: "17" },
  { label: "checkVisibility", re: /\.checkVisibility\s*\(/g, since: "17.4" },
  { label: "scrollend", re: /["']scrollend["']/g, since: "18" },
  // syntax esbuild should have lowered for the safari13 build target
  { label: "logical assignment", re: /[?|&]{2}=[^=]/g, since: "14" },
  { label: "static init block", re: /\bstatic\s*\{/g, since: "16.4" },
];

// A hit reached through a typeof/optional-call check is a fallback path.
const GUARD = /typeof |\?\.|&&|\|\||catch/;

const dir = process.argv[2] ?? "dist/assets";
let failed = 0;

for (const file of readdirSync(dir).filter(f => f.endsWith(".js"))) {
  const source = readFileSync(join(dir, file), "utf8");
  const rows = [];
  for (const check of CHECKS) {
    check.re.lastIndex = 0;
    let hit;
    let total = 0;
    let bare = 0;
    let sample = "";
    while ((hit = check.re.exec(source))) {
      total++;
      const around = source.slice(Math.max(0, hit.index - 60), hit.index + 40);
      if (GUARD.test(around)) continue;
      bare++;
      if (!sample) sample = around.replace(/\s+/g, " ");
    }
    if (bare) rows.push({ ...check, total, bare, sample });
  }
  if (!rows.length) continue;
  console.log(`\n${file}`);
  for (const row of rows) {
    const cover = row.cover ?? "UNPATCHED";
    if (!row.cover) failed++;
    console.log(`  safari ${row.since.padEnd(4)} ${row.label.padEnd(24)} ${String(row.bare).padStart(2)}/${row.total}  ${cover}${row.why ? ` (${row.why})` : ""}`);
    if (!row.cover) console.log(`      …${row.sample}…`);
  }
}

console.log(failed ? `\n${failed} unpatched API(s)` : `\nno unpatched post-Safari-13.1 APIs in ${dir}`);
process.exit(failed ? 1 : 0);
