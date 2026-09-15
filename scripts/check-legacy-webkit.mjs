// Re-derives the post-floor API list from a build and fails on anything unpatched.
//
// Two contracts are checked, because the repo ships two frontends:
//
//   dist/         the desktop control UI, whose floor is Safari 13.1 -- the WKWebView on
//                 macOS 10.15. JS, CSS and HTML.
//   static/       the client web page, served to whatever browser a phone happens to have.
//                 Its floor is older still and it has no build step, so anything modern in
//                 it ships verbatim.
//
// A check is satisfied either by a `cover` (a shim, a fallback, or an argument that the
// construct is unreachable) or by not matching. `cover: "shim"` additionally names the
// function in src/lib/legacy-webview-polyfills.ts that provides it, and the run fails if
// that function is no longer defined and called -- otherwise deleting a shim would leave
// the checker cheerfully printing the coverage it no longer has.

import { readdirSync, readFileSync, existsSync, statSync } from "node:fs";
import { join, extname, relative, sep } from "node:path";

const POLYFILLS = "src/lib/legacy-webview-polyfills.ts";
const CLIENT_DIR = join("src-tauri", "src", "streamer", "static");

// Hits that are accounted for but whose reason is specific to one file rather than to the
// check. `file` is matched as a basename prefix, so it survives the content hash; `max` is
// a ceiling, so a new hit in the same file still fails while fixing them all does not.
const KNOWN_DEBT = {
  ":has()": [
    { file: "main-", max: 1, why: "the table kit's [&:has([role=checkbox])]; nothing in src/ renders role=checkbox" },
    { file: "monaco-setup", max: 1, why: "monaco's markdown renderer styling a task-list item; the config editor renders no markdown" },
  ],
  "logical margin/padding": [
    { file: "monaco-setup", max: 1, why: "monaco's own stylesheet: margin-block:0 on the overlay message box" },
  ],
  "text-wrap": [
    { file: "monaco-setup", max: 1, why: "monaco's codelens styling; codeLens is off in editorOptions" },
  ],
  "Error cause": [
    { file: "monaco-setup", max: 1, why: "monaco's worker-post failure path; an unknown options bag is ignored, the Error still constructs" },
  ],
};

const JS_CHECKS = [
  { label: "WeakRef", re: /\bnew WeakRef\s*\(/g, since: "14.1", cover: "shim", shim: "shimWeakRef" },
  { label: "FinalizationRegistry", re: /\bFinalizationRegistry\b/g, since: "14.1" },
  { label: "Promise.any", re: /\bPromise\s*\.\s*any\s*\(/g, since: "14" },
  { label: "AggregateError", re: /\bnew AggregateError\s*\(/g, since: "14", cover: "shim", shim: "shimAggregateError" },
  { label: "new EventTarget()", re: /\bnew EventTarget\s*\(/g, since: "14" },
  { label: "replaceChildren", re: /\.replaceChildren\s*\(/g, since: "14", cover: "shim", shim: "shimReplaceChildren" },
  {
    label: "MediaQueryList events",
    re: /matchMedia\([^)]*\)\s*\.\s*(add|remove)EventListener/g,
    since: "14",
    cover: "shim",
    shim: "bridgeMediaQueryListEvents",
  },
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
  { label: "structuredClone", re: /\bstructuredClone\s*\(/g, since: "15.4", guard: /typeof\s+(globalThis\s*\.\s*)?structuredClone/ },
  { label: "relative indexing .at()", re: /\.at\s*\(/g, since: "15.4", cover: "shim", shim: "shimRelativeIndexing" },
  { label: "findLast/findLastIndex", re: /\.findLast(?:Index)?\s*\(/g, since: "15.4", cover: "shim", shim: "shimFindLast" },
  { label: "crypto.randomUUID", re: /\brandomUUID\s*\(/g, since: "15.4", guard: /typeof[^;{}]{0,80}randomUUID/ },
  { label: "dialog.showModal", re: /\.showModal\s*\(/g, since: "15.4" },
  { label: "BroadcastChannel", re: /\bnew BroadcastChannel\b/g, since: "15.4" },
  { label: "AbortSignal.abort/timeout", re: /\bAbortSignal\s*\.\s*(abort|timeout)\s*\(/g, since: "15.4" },
  { label: "navigator.locks", re: /\bnavigator\s*\.\s*locks\b/g, since: "15.4" },
  { label: "reportError", re: /\breportError\s*\(/g, since: "15.4" },
  { label: "requestIdleCallback", re: /\brequestIdleCallback\s*\(/g, since: "15.4", cover: "shim", shim: "shimRequestIdleCallback" },
  { label: "CSS.registerProperty", re: /\bCSS\s*\.\s*registerProperty\b/g, since: "16.4" },
  { label: "ElementInternals", re: /\.attachInternals\s*\(/g, since: "16.4" },
  { label: "TextDecoderStream", re: /\bnew Text(?:Decoder|Encoder)Stream\b/g, since: "16.4" },
  { label: "Array.fromAsync", re: /\bArray\s*\.\s*fromAsync\b/g, since: "16.4" },
  { label: "toSorted/toReversed", re: /\.(toSorted|toReversed|toSpliced)\s*\(/g, since: "16.4" },
  { label: "Object.groupBy", re: /\bObject\s*\.\s*groupBy\b/g, since: "17.4" },
  { label: "Promise.withResolvers", re: /\bwithResolvers\s*\(/g, since: "17.4" },
  { label: "URL.canParse", re: /\bURL\s*\.\s*canParse\b/g, since: "17" },
  { label: "checkVisibility", re: /\.checkVisibility\s*\(/g, since: "17.4" },
  // Anchored on the write, not on the identifier: `inert` is also a React prop name that
  // react-remove-scroll passes around, and that is not the DOM attribute. This is the one
  // Base UI reaches for in every focus-managing component (`floating-ui-react/utils/
  // markOthers`), and it degrades silently -- focus simply is not trapped, no error.
  { label: "inert attribute", re: /(?:set|toggle|remove)Attribute\s*\(\s*["']inert["']|\.inert\s*=[^=]/g, since: "15.5" },
  { label: "scrollend", re: /["']scrollend["']/g, since: "18" },
  {
    label: "RegExp d/v flag",
    hard: true,
    re: /new RegExp\([^;]{0,300}?["'][gimsuy]*[dv][gimsuy]*["']\s*\)/g,
    since: "17",
    cover: "shim",
    shim: "shimRegExpFlags",
    why: "stripped by shimRegExpFlags, which rebuilds match.indices",
  },
  { label: "logical assignment", re: /[?|&]{2}=[^=]/g, since: "14", hard: true },
  { label: "static init block", re: /\bstatic\s*\{/g, since: "16.4", hard: true },
  { label: "class field", re: /\bclass\b[^{;()]{0,60}\{\s*(?:static\s+)?[A-Za-z_$][\w$]*\s*[;=][^=>]/g, since: "14", hard: true },
  { label: "private class member", re: /[{;]\s*#[A-Za-z_$]/g, since: "14.1", hard: true },
  // A parse-time SyntaxError in a regex literal: it kills the whole chunk before any of it
  // runs, so no amount of runtime guarding helps.
  { label: "RegExp lookbehind", re: /\(\?<[=!]/g, since: "16.4", hard: true, cover: "unreachable", why: "monaco's only use is behind its own lazy `new RegExp` wrapper with a try/catch" },
  { label: "module worker", re: /new Worker\([^)]{0,200}?type\s*:\s*["']module["']/g, since: "15", hard: true },
  { label: "import.meta.resolve", re: /import\s*\.\s*meta\s*\.\s*resolve/g, since: "16.4", hard: true },
];

const CSS_CHECKS = [
  { label: ":is()", re: /:is\(/g, since: "14" },
  {
    label: ":where()",
    re: /:where\(/g,
    since: "14",
    cover: "fallback",
    why: "tailwind preflight only; src/index.css re-states the [hidden] and input[type=button] resets on the floor, and the rtl: utilities have no locale to fire in",
  },
  { label: ":has()", re: /:has\(/g, since: "15.4" },
  {
    label: ":focus-visible",
    re: /:focus-visible/g,
    since: "15.4",
    cover: "fallback",
    why: "the UA outline is suppressed in a plain :focus rule on both pages (src/index.css, static/styles.css); the :focus-visible rule beside it only re-suppresses on engines that have it, and an engine that cannot parse it drops that rule alone",
  },
  {
    label: "aspect-ratio",
    re: /aspect-ratio\s*:/g,
    since: "15",
    cover: "unreachable",
    why: ".aspect-square only lands on AvatarImage, which is already sized by h-full w-full",
  },
  {
    label: "logical inset",
    re: /inset-(?:inline|block)(?:-\w+)?\s*:/g,
    since: "14.1",
    cover: "fallback",
    why: "src/index.css gives .start-1 a left fallback on the floor; the app ships LTR only",
  },
  { label: "logical margin/padding", re: /(?:margin|padding)-(?:inline|block)(?:-\w+)?\s*:/g, since: "14.1" },
  { label: "inset shorthand", re: /[;{]\s*inset\s*:/g, since: "14.1" },
  { label: "@container", re: /@container\b|container-type\s*:/g, since: "16" },
  { label: "color-mix()", re: /color-mix\(/g, since: "16.2" },
  { label: "@layer", re: /@layer\b/g, since: "15.4" },
  { label: ":dir()", re: /:dir\(/g, since: "16.4" },
  { label: "text-wrap", re: /text-wrap\s*:/g, since: "17.4" },
  {
    label: "accent-color",
    re: /accent-color\s*:/g,
    since: "15.4",
    cover: "capability probe",
    why: "only inside the `@supports not (accent-color: red)` guard that gates the floor repairs",
  },
  { label: "dvh/svh/lvh units", re: /\d(?:dvh|svh|lvh|dvw|svw|lvw)\b/g, since: "15.4" },
  { label: ":focus-within", re: /:focus-within/g, since: "10.1", cover: "supported", why: "shipped in safari 10.1, below the floor" },
];

// The client page is hand-written and unbundled, so syntax matters as much as APIs.
const CLIENT_JS_CHECKS = [
  { label: "optional chaining", re: /\?\.[\w[(]/g, since: "13.4", hard: true },
  { label: "nullish coalescing", re: /\?\?[^=]/g, since: "13.4", hard: true },
  { label: "logical assignment", re: /[?|&]{2}=[^=]/g, since: "14", hard: true },
  { label: "class field", re: /\bclass\b[^{;()]{0,60}\{\s*(?:static\s+)?[A-Za-z_$][\w$]*\s*[;=][^=>]/g, since: "14", hard: true },
  { label: "private class member", re: /[{;]\s*#[A-Za-z_$]/g, since: "14.1", hard: true },
  { label: "static init block", re: /\bstatic\s*\{/g, since: "16.4", hard: true },
  { label: "top-level await", re: /^await[\s(]/gm, since: "15", hard: true },
  { label: "String.replaceAll", re: /\.replaceAll\s*\(/g, since: "13.4" },
  { label: "relative indexing .at()", re: /\.at\s*\(/g, since: "15.4" },
  { label: "findLast/findLastIndex", re: /\.findLast(?:Index)?\s*\(/g, since: "15.4" },
  { label: "Object.fromEntries", re: /\bObject\s*\.\s*fromEntries\b/g, since: "12.2" },
  { label: "Promise.allSettled", re: /\bPromise\s*\.\s*allSettled\b/g, since: "13" },
  { label: "Promise.any", re: /\bPromise\s*\.\s*any\s*\(/g, since: "14" },
  { label: "structuredClone", re: /\bstructuredClone\s*\(/g, since: "15.4", guard: /typeof\s+structuredClone/ },
  { label: "BroadcastChannel", re: /\bnew BroadcastChannel\b/g, since: "15.4" },
  { label: "AbortSignal.abort/timeout", re: /\bAbortSignal\s*\.\s*(?:abort|timeout)\s*\(/g, since: "15.4" },
  { label: "requestIdleCallback", re: /\brequestIdleCallback\s*\(/g, since: "15.4", guard: /typeof[^;{}]{0,80}requestIdleCallback/ },
  { label: "RegExp lookbehind", re: /\(\?<[=!]/g, since: "16.4", hard: true },
  { label: "module worker", re: /new Worker\([^)]{0,200}?type\s*:\s*["']module["']/g, since: "15", hard: true },
];

const CLIENT_CSS_CHECKS = [
  { label: ":is()", re: /:is\(/g, since: "14" },
  { label: ":where()", re: /:where\(/g, since: "14" },
  { label: ":has()", re: /:has\(/g, since: "15.4" },
  { label: "inset shorthand", re: /[;{]\s*inset\s*:/g, since: "14.1" },
  { label: "logical inset", re: /inset-(?:inline|block)(?:-\w+)?\s*:/g, since: "14.1" },
  { label: "aspect-ratio", re: /aspect-ratio\s*:/g, since: "15" },
  { label: "@container", re: /@container\b|container-type\s*:/g, since: "16" },
  { label: "color-mix()", re: /color-mix\(/g, since: "16.2" },
  { label: "dvh/svh/lvh units", re: /\d(?:dvh|svh|lvh|dvw|svw|lvw)\b/g, since: "15.4" },
  { label: "gap in flex", re: /display\s*:\s*(?:inline-)?flex[^}]*?[;{]\s*(?:row-|column-)?gap\s*:/g, since: "14.1" },
];

// Reading a class attribute is the only way to see a flex container and a gap utility in
// the same box: tailwind emits .flex and .gap-4 as separate rules, so the built CSS cannot
// answer this. Safari 13.1 has gap for grid but not for flex.
const FLEX_GAP_RE = /className\s*=\s*(?:"([^"]*)"|'([^']*)'|\{`([^`]*)`\}|\{"([^"]*)"\})/g;

function walk(dir, out = []) {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) walk(full, out);
    else out.push(full);
  }
  return out;
}

// The statement the hit sits in, so a guard cannot be satisfied by an unrelated `&&` sixty
// characters away in minified output.
function statementAround(source, index) {
  let start = index;
  while (start > 0 && index - start < 400 && !";{}".includes(source[start - 1])) start--;
  let end = index;
  while (end < source.length && end - index < 200 && !";{}".includes(source[end])) end++;
  return source.slice(start, end);
}

function scan(source, checks) {
  const rows = [];
  for (const check of checks) {
    check.re.lastIndex = 0;
    let hit;
    let total = 0;
    let bare = 0;
    let suppressed = 0;
    let sample = "";
    while ((hit = check.re.exec(source))) {
      total++;
      if (check.guard && !check.hard && check.guard.test(statementAround(source, hit.index))) {
        suppressed++;
        continue;
      }
      bare++;
      if (!sample) {
        sample = source.slice(Math.max(0, hit.index - 60), hit.index + 40).replace(/\s+/g, " ");
      }
    }
    if (bare) rows.push({ check, total, bare, suppressed, sample });
  }
  return rows;
}

function report(label, rows) {
  if (!rows.length) return 0;
  console.log(`\n${label}`);
  let failed = 0;
  for (const { check, total, bare, suppressed, sample } of rows) {
    const cover = check.cover ?? "UNPATCHED";
    if (!check.cover) failed++;
    const counts = `${String(bare).padStart(2)}/${total}${suppressed ? ` (-${suppressed} guarded)` : ""}`;
    console.log(`  safari ${check.since.padEnd(4)} ${check.label.padEnd(24)} ${counts}  ${cover}${check.why ? ` (${check.why})` : ""}`);
    if (!check.cover) console.log(`      …${sample}…`);
  }
  return failed;
}

// A `cover: "shim"` claim is only worth anything while the shim is still there.
function verifyShims(checks) {
  const named = [...new Set(checks.filter(c => c.cover === "shim").map(c => c.shim))];
  const missingName = checks.find(c => c.cover === "shim" && !c.shim);
  if (missingName) {
    console.log(`\ncover: "shim" without a shim name: ${missingName.label}`);
    return 1;
  }
  if (!existsSync(POLYFILLS)) {
    console.log(`\n${POLYFILLS} is missing; every cover: "shim" claim is unbacked`);
    return named.length;
  }
  const source = readFileSync(POLYFILLS, "utf8");
  let failed = 0;
  for (const name of named) {
    const defined = new RegExp(`function\\s+${name}\\s*\\(`).test(source);
    const called = new RegExp(`^${name}\\s*\\(\\s*\\)\\s*;`, "m").test(source);
    if (defined && called) continue;
    console.log(`\n${POLYFILLS}: ${name} is ${defined ? "defined but never called" : "gone"}, but a check claims it as cover`);
    failed++;
  }
  return failed;
}

function checkFlexGap() {
  if (!existsSync("src")) return 0;
  let failed = 0;
  for (const file of walk("src")) {
    if (![".tsx", ".ts"].includes(extname(file))) continue;
    const source = readFileSync(file, "utf8");
    FLEX_GAP_RE.lastIndex = 0;
    let hit;
    while ((hit = FLEX_GAP_RE.exec(source))) {
      const classes = (hit[1] ?? hit[2] ?? hit[3] ?? hit[4] ?? "").split(/\s+/);
      const flex = classes.some(c => c === "flex" || c === "inline-flex");
      const grid = classes.some(c => c === "grid" || c === "inline-grid");
      const gap = classes.some(c => /^(?:sm:|md:|lg:|xl:)?(?:row-|column-)?gap-/.test(c));
      if (flex && gap && !grid) {
        console.log(`\n${file}: flex container with a gap-* utility (safari 14.1; grid has it, flex does not)`);
        console.log(`      …${hit[0].slice(0, 140)}…`);
        failed++;
      }
    }
  }
  return failed;
}

function allowance(label, name) {
  return (KNOWN_DEBT[label] ?? []).find(entry => name.startsWith(entry.file));
}

function scanTree(dir, jsChecks, cssChecks) {
  let failed = 0;
  for (const file of walk(dir)) {
    const ext = extname(file);
    const checks = ext === ".css" ? cssChecks : ext === ".js" || ext === ".mjs" || ext === ".html" ? jsChecks : null;
    if (!checks) continue;
    const source = readFileSync(file, "utf8");
    const name = file.split(sep).pop();
    const rows = scan(source, ext === ".html" ? [...jsChecks, ...cssChecks] : checks);
    for (const row of rows) {
      const allowed = allowance(row.check.label, name);
      if (allowed && row.bare <= allowed.max) {
        row.check = { ...row.check, cover: "known debt", why: allowed.why };
      }
    }
    failed += report(relative(".", file), rows);
  }
  return failed;
}

const dir = process.argv[2] ?? "dist";
let failed = 0;

if (!existsSync(dir)) {
  console.log(`${dir} does not exist; run \`pnpm build\` first`);
  process.exit(1);
}

failed += scanTree(dir, JS_CHECKS, CSS_CHECKS);
failed += verifyShims(JS_CHECKS);
failed += checkFlexGap();

if (existsSync(CLIENT_DIR)) {
  console.log(`\n--- client page (${CLIENT_DIR}) ---`);
  failed += scanTree(CLIENT_DIR, CLIENT_JS_CHECKS, CLIENT_CSS_CHECKS);
}

console.log(failed ? `\n${failed} unpatched API(s)` : `\nno unpatched post-floor APIs in ${dir} or the client page`);
process.exit(failed ? 1 : 0);
