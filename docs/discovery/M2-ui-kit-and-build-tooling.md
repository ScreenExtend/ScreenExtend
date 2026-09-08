# M2 — UI kit and build tooling

Territory: `src/components/ui/` (all 23 files), `src/layout/`, `src/assets/`, `index.html`,
`vite.config.ts`, `tailwind.config.js`, `postcss.config.js`, `eslint.config.js`, `tsconfig.json`,
`tsconfig.node.json`, `components.json`, `scripts/check-legacy-webkit.mjs`, `src/index.css`,
`package.json` dependencies.

Every file listed was read in full. Measurements come from the checked-in build in `dist/`
(produced 2026-09-04; `dist/` is gitignored via `.gitignore:11`). Where a claim is inferred
rather than measured it is marked **unverified**.

Two structural notes before the sections. First, this territory sits on the **host control-plane
path**, not the glass-to-glass video path — no frame, audio sample or input event passes through
any file here. Its latency contribution is app *startup* and main-thread contention in the Tauri
webview while streaming. Sections 3–7 (threads, locks, buffers, queues, FFI) are therefore
largely N/A and are answered honestly as such rather than padded. Second, `CLAUDE.md` and the
code disagree in three places, recorded in §12.

---

## 1. Files and purpose

### Build and tooling configuration

**`vite.config.ts`** (130 lines) — the whole frontend build. Three concerns interleaved.
`monacoDeps()` (`:8-17`) scrapes `src/lib/monaco-setup.ts` and `src/components/json-editor.tsx`
for `monaco-editor/esm/…` import specifiers with a regex and feeds them to
`optimizeDeps.include`, so Vite pre-bundles the exact monaco surface the app touches.
`classicWorkers()` (`:25-81`) is a **dev-server-only** plugin (`apply: "serve"`) that serves
`/@classic-worker/<name>.js` by running a nested Vite `build()` with `formats: ["iife"]` and
`target: "safari13"`, memoised in a module-level `Map` — it exists because Vite's dev server
hands out `?worker` imports as ESM module workers and Safari 13.1 cannot construct one. The
exported config sets `build.target: ["es2020","chrome87","safari13"]`, `esbuild.target:
"safari13"`, `optimizeDeps.esbuildOptions.target: "safari13"`, the `@` → `src` alias and the
load-bearing `next/navigation` → `src/lib/next-navigation-stub.ts` alias for `nextstepjs`, and
the Tauri dev-server settings (fixed port 1420, `strictPort`, `clearScreen: false`,
`watch.ignored: ["**/src-tauri/**"]`). Notably **absent**: `manualChunks`, `worker.format`,
`build.sourcemap`, `build.minify`, `build.chunkSizeWarningLimit`.

**`tailwind.config.js`** (82 lines) — `darkMode: ["variant", ".dark &"]` (the documented
workaround for Tailwind 3.4's default `:is(.dark *)`, which Safari 13.1 drops); four `content`
globs of which three point at directories that do not exist; the shadcn HSL-variable colour map;
`borderRadius` derived from `--radius`; three keyframe/animation pairs (`accordion-down`,
`accordion-up`, `caret-blink`); `tailwindcss-animate` as the only plugin.

**`postcss.config.js`** (6 lines) — `tailwindcss` then `autoprefixer`, nothing else. Autoprefixer
reads `package.json:6-9` `browserslist: ["safari >= 13.1", "chrome >= 87"]`.

**`eslint.config.js`** (44 lines) — ESLint 9 flat config. One ignore block, one rules block
scoped to `src/**/*.{ts,tsx}`. Plugins: `@typescript-eslint` (recommended, **not** type-aware),
`react-hooks`, `react-refresh`. `rules-of-hooks` error; `exhaustive-deps`, `no-explicit-any`,
`no-unused-vars` warn.

**`tsconfig.json`** (30 lines) — `target: ES2020`, `useDefineForClassFields: true`,
`moduleResolution: "bundler"`, `strict`, `noUnusedLocals`, `noUnusedParameters`,
`noFallthroughCasesInSwitch`, `noEmit`, `@/*` path alias, `include: ["src"]`, project reference
to `tsconfig.node.json`. **`tsconfig.node.json`** (10 lines) covers `vite.config.ts` only.

**`components.json`** (17 lines) — shadcn generator config: `style: "default"`, `rsc: false`,
`baseColor: "slate"`, `cssVariables: true`, aliases `@/components` and `@/lib/utils`. Purely an
input to `npx shadcn add`; nothing reads it at build time.

**`scripts/check-legacy-webkit.mjs`** (85 lines) — the Safari-13.1 gate. A `CHECKS` array of 38
`{label, re, since, cover?, why?, hard?}` entries, a `GUARD` regex, and a loop over
`dist/assets/*.js` that reports each hit and exits 1 if any check without a `cover` field fires.
Analysed in detail in §14.

**`index.html`** (59 lines) — the Vite entry. Beyond the usual head tags it carries three inline
classic `<script>` blocks that run before the deferred module entry: a `prefers-color-scheme`
bootstrap that stamps `dark`/`light` on `<html>`; a chrome-suppression block
(`contextmenu`/modified-`click`/`F5`/`Ctrl+R`/`dragover`/`drop`); and a `window.error` handler
that paints a "ScreenExtend could not start … install Safari 13.1 or newer" panel into `#root`
using only DOM APIs and `style.cssText` (deliberately dependency-free, since it must run when the
bundle itself failed).

**`src/index.css`** (135 lines) — the only global stylesheet. Tailwind's three `@tailwind`
directives, the light/dark HSL token blocks, `* { @apply border-border }`, the app font stack, a
custom `-webkit-scrollbar` theme, `body` positioning (`position: fixed; overflow: hidden` — the
app never scrolls the document), number-input spinner removal, a global
`button, input { transition: all 200ms !important }`, a `button` appearance reset, four
Monaco-specific overrides, and the `nextstep-pointer` ring.

### `src/components/ui/` — the kit (23 files)

All are shadcn/Radix-derived and follow the same shape (`React.forwardRef` + `cva` variants +
`cn()` class merge), with local edits noted.

| File | Purpose | Local deviation from upstream |
|---|---|---|
| `alert-dialog.tsx` | Radix AlertDialog wrapper | adds `addChildTopMargins` (`:13-24`) which clones children to inject `mt-4` — a `space-y` substitute, consistent with the flex-`gap` ban |
| `avatar.tsx` | Radix Avatar | stock |
| `badge.tsx` | `cva` badge, 4 variants | stock |
| `button.tsx` | `cva` button, 6 variants × 4 sizes, `asChild` via `Slot` | appends `hover:opacity-75` **after** the caller's className (`:47`) |
| `card.tsx` | Card/Header/Title/Description/Content/Footer | `CardHeader` is `flex-row items-center justify-between` (upstream is `flex-col`) |
| `carousel.tsx` | embla wrapper + context + prev/next buttons | three `@ts-expect-error type-err` on the embla type imports (`:3,5,7`) |
| `checkbox.tsx` | Radix Checkbox | stock |
| `dropdown-menu.tsx` | Radix DropdownMenu, full surface | `data-[disabled]` uses `opacity-50` without `pointer-events-none` on `Item` (`:84`) |
| `form.tsx` | react-hook-form bindings | **zero importers** |
| `input-otp.tsx` | `input-otp` wrapper | container uses `flex items-center` with no gap (slots are joined by borders) |
| `input.tsx` | hand-written input with an optional floating label | not shadcn at all; `forwardRef` render fn takes one arg, so the ref is dropped |
| `label.tsx` | Radix Label + `cva` | stock |
| `resizable.tsx` | `react-resizable-panels` wrapper | **zero importers**; 4-space indent, unlike the rest of the kit |
| `select.tsx` | Radix Select | stock |
| `sheet.tsx` | Radix Dialog as a side sheet, 4 sides | declares an unused `trapFocus?: boolean` prop (`:51`) |
| `slider.tsx` | Radix Slider | stock |
| `switch.tsx` | Radix Switch | stock |
| `table.tsx` | semantic table primitives | `[&:has([role=checkbox])]:pr-0` on head and cell |
| `toast.tsx` | Radix Toast + `cva` | stock |
| `toaster.tsx` | renders the `useToast` queue | wraps title+description in a `<div>` and uses `mt-1` instead of a gap |
| `tooltip.tsx` | **Base UI** (not Radix) tooltip | the only Base UI component; written in Tailwind **v4** class syntax |
| `use-toast.ts` | module-level toast store + reducer + `useToast` | upstream verbatim, including `TOAST_REMOVE_DELAY = 1000000` |

Overrides worth calling out as a group: `alert-dialog.tsx:32`, `sheet.tsx:22` and
`input-otp.tsx:52` all spell out `fixed top-0 right-0 bottom-0 left-0` rather than `inset-0`,
because the `inset` shorthand is Safari 14.1. That is a deliberate, correct, undocumented
convention.

### `src/layout/`

**`layout.tsx`** (61 lines) — the app shell. A flex row: a collapsible sidebar column whose
`width` is transitioned between 275 and 0 via inline style, and a content column holding a header
strip (sidebar toggle + `ProfileMenu`), the routed `children`, and a footer strip showing the
session id/OTP and the theme toggle. Reads `windowOtp` and `windowSessionId` from
`GlobalProviderContext`.

**`sidebar.tsx`** (67 lines) — brand heading plus three `<Link>`-wrapped `<Button>`s (Add Device
/ Edit Device / Settings) whose `variant` flips on `location.pathname`. Carries `lg:` responsive
classes and a `ChevronLeft` with `id="hideArrow"` that has no handler.

### `src/assets/`

Three SVGs, all inert data. `logo.svg` (765 B, referenced from `index.html:5` as the favicon and
emitted as `dist/assets/logo-0liObS7h.svg`), `default.svg` (1,804 B — a generic person glyph, the
avatar fallback), `illustration.svg` (27,935 B — a large multi-group illustration). No raster
assets, no fonts here (the font comes from the `non.geist` package).

---

## 2. Function catalog

### `vite.config.ts`
- `monacoDeps(): string[]` (`:8`) — regex-scrapes two source files for `monaco-editor/esm/…`
  specifiers, seeded with `@monaco-editor/react`; returns the deduped list. Unguarded
  `readFileSync`, so a rename breaks every Vite command at config-load time.
- `classicWorkers(): Plugin` (`:25`) — dev-only middleware factory. Closes over a `bundled`
  `Map<string,string>` that is never invalidated.
- `configureServer(server)` (`:30`) — the middleware. Parses the worker name out of the URL,
  looks it up in `WORKER_ENTRIES`, serves the cached bundle or runs a nested `build()` and caches
  it. Errors go to `next(error)`.
- `send(code)` (`:41`) — sets `Content-Type: text/javascript` and ends the response.
- default export (`:84`) — an `async` config factory (nothing in it awaits).

### `scripts/check-legacy-webkit.mjs`
- module body (`:53-85`) — the only executable code; no functions. Reads `process.argv[2] ??
  "dist/assets"`, iterates `.js` files, runs every `CHECKS[i].re` over the source, applies
  `GUARD` to a ±100-char window around each hit, collects unguarded hits into `rows`, prints one
  line per row, increments `failed` for rows without a `cover`, and `process.exit(failed ? 1 : 0)`.

### `src/index.css`
No functions (CSS).

### `src/components/ui/` — components

`alert-dialog.tsx`: `addChildTopMargins(children)` (`:13`) — `React.Children.map` + `cloneElement`
adding `mt-4` to every child after the first. Re-exports `AlertDialog`/`Trigger`/`Portal` as
Radix aliases; `AlertDialogOverlay` (`:26`), `AlertDialogContent` (`:41`, portals + overlay +
content and pipes children through `addChildTopMargins`), `AlertDialogHeader` (`:61`),
`AlertDialogFooter` (`:75`), `AlertDialogTitle` (`:89`), `AlertDialogDescription` (`:101`),
`AlertDialogAction` (`:114`, styled with `buttonVariants()`), `AlertDialogCancel` (`:126`,
`buttonVariants({variant:"outline"})`).

`avatar.tsx`: `Avatar` (`:6`), `AvatarImage` (`:21`), `AvatarFallback` (`:33`) — thin styled
forwards.

`badge.tsx`: `badgeVariants` (`:6`, `cva`), `Badge` (`:30`, a plain function, not `forwardRef`).

`button.tsx`: `buttonVariants` (`:7`, `cva`: 6 variants, 4 sizes), `Button` (`:42`, swaps `button`
for Radix `Slot` when `asChild`).

`card.tsx`: `Card` (`:5`), `CardHeader` (`:20`), `CardTitle` (`:32`, renders `<h3>`),
`CardDescription` (`:47`), `CardContent` (`:59`), `CardFooter` (`:67`).

`carousel.tsx`: `useCarousel()` (`:33`, throws outside a `<Carousel>`); `Carousel` (`:43`) —
calls `useEmblaCarousel`, owns `canScrollPrev`/`canScrollNext` state, `onSelect` (`:69`),
`scrollPrev`/`scrollNext` (`:78`,`:82`), `handleKeyDown` (`:86`, arrow keys), an effect exposing
the api to `setApi` (`:99`), and an effect binding `reInit`/`select` (`:107`) — note its cleanup
at `:116-118` unbinds only `select`, **leaking the `reInit` handler**. `CarouselContent` (`:151`),
`CarouselItem` (`:173`), `CarouselPrevious` (`:195`), `CarouselNext` (`:224`).

`checkbox.tsx`: `Checkbox` (`:7`).

`dropdown-menu.tsx`: aliases for `Root`/`Trigger`/`Group`/`Portal`/`Sub`/`RadioGroup`; styled
`DropdownMenuSubTrigger` (`:19`), `SubContent` (`:41`), `Content` (`:57`, self-portalling),
`Item` (`:75`), `CheckboxItem` (`:93`), `RadioItem` (`:117`), `Label` (`:139`), `Separator`
(`:157`), `Shortcut` (`:169`).

`form.tsx`: `Form` = `FormProvider`; `FormField` (`:29`); `useFormField()` (`:42`, see §8);
`FormItem` (`:73`, `React.useId`); `FormLabel` (`:87`); `FormControl` (`:104`, `Slot` + aria
wiring); `FormDescription` (`:126`); `FormMessage` (`:143`, returns `null` when empty).

`input-otp.tsx`: `InputOTP` (`:9`), `InputOTPGroup` (`:25`), `InputOTPSlot` (`:33`, unguarded
`inputOTPContext.slots[index]`), `InputOTPSeparator` (`:61`).

`input.tsx`: `Input` (`:8`) — branches on `hoverLabel`; the true branch renders a relative wrapper
with a peer-driven floating `<label>`, the false branch a bare input. Both use the internal
`useFocus` ref; the forwarded ref is dropped.

`label.tsx`: `labelVariants` (`:7`, a `cva` with no variants), `Label` (`:11`).

`resizable.tsx`: `ResizablePanelGroup` (`:7`), `ResizablePanel` (`:20`, a re-export),
`ResizableHandle` (`:22`, optional grip).

`select.tsx`: `SelectTrigger` (`:13`), `SelectScrollUpButton` (`:33`), `SelectScrollDownButton`
(`:50`), `SelectContent` (`:68`, portalled, `position="popper"` default), `SelectLabel` (`:100`),
`SelectItem` (`:112`), `SelectSeparator` (`:135`).

`sheet.tsx`: `SheetOverlay` (`:16`), `sheetVariants` (`:31`, `cva` over 4 sides), `SheetContent`
(`:54`, portal + overlay + close button), `SheetHeader` (`:75`), `SheetFooter` (`:89`),
`SheetTitle` (`:103`), `SheetDescription` (`:115`).

`slider.tsx`: `Slider` (`:6`) — Root/Track/Range/Thumb, single thumb hardcoded.

`switch.tsx`: `Switch` (`:6`) — thumb animates via `transition-transform` + `translate-x-5`
(compositor-friendly, correct).

`table.tsx`: `Table` (`:5`, wraps in `overflow-auto`), `TableHeader` (`:19`), `TableBody` (`:27`),
`TableFooter` (`:39`), `TableRow` (`:54`), `TableHead` (`:69`), `TableCell` (`:84`),
`TableCaption` (`:96`).

`toast.tsx`: `ToastViewport` (`:10`), `toastVariants` (`:25`), `Toast` (`:41`), `ToastAction`
(`:56`), `ToastClose` (`:71`), `ToastTitle` (`:89`), `ToastDescription` (`:101`).

`toaster.tsx`: `Toaster()` (`:11`) — maps `useToast().toasts` into `<Toast>` elements.

`tooltip.tsx`: `TooltipProvider` (`:7`, `delay = 0`), `Tooltip` (`:20`), `TooltipTrigger` (`:24`),
`TooltipContent` (`:28`, portal + positioner + popup + arrow). All plain functions — no
`forwardRef`, matching Base UI's convention.

`use-toast.ts`: `genId()` (`:28`, monotonic counter mod `MAX_SAFE_INTEGER`); `addToRemoveQueue(id)`
(`:59`); `reducer(state, action)` (`:75`, exported); `dispatch(action)` (`:132`, mutates
`memoryState` and notifies listeners); `toast(props)` (`:141`, returns `{id, dismiss, update}`);
`useToast()` (`:170`).

### `src/layout/`
- `Layout({children})` (`layout.tsx:12`) — default export; owns `sidebarOpen` state.
- `Sidebar()` (`sidebar.tsx:11`) — reads `useLocation()`; no state.

### `index.html` (inline)
Three anonymous IIFEs/handlers: the theme bootstrap (`:9-16`), the four chrome-suppression
listeners (`:19-31`), and the `window.error` boot-failure renderer (`:34-52`).

---

## 3. Threads

None. This territory is single-threaded browser JavaScript on the webview main thread, with two
exceptions, neither owned here:

- **Monaco web workers.** `dist/assets/monaco-editor.worker-ByQk1jEG.js` (237 KB) and
  `monaco-json.worker-UHvWpAyk.js` (370 KB) are real OS threads spawned by
  `src/lib/monaco-setup.ts:33-38`. The build path constructs them via Vite's `?worker` import
  (classic scripts, because Vite 6's `worker.format` default is `"iife"`); the dev path uses
  `new Worker("/@classic-worker/" + name + ".js")` served by `vite.config.ts`'s
  `classicWorkers()` plugin. **The production format is not pinned anywhere** — see the finding
  `major-build-vite-worker-format-unpinned`. Lifetime is the app's; `src/lib/preload-editor.ts`
  warms them.
- **`classicWorkers()`'s nested build** (`vite.config.ts:52-77`) runs Rollup/esbuild inside the
  dev server process on first worker request. Dev-only; blocking for the duration of that
  request; result memoised forever in a `Map`.

No priority/QoS control exists or is applicable.

---

## 4. Locks

None. There are no mutexes, no atomics, no `SharedArrayBuffer`, and no `navigator.locks` usage
anywhere in this territory (grep for `navigator.locks` over `dist/assets/*.js` returns zero, and
`scripts/check-legacy-webkit.mjs:30` checks for it precisely because it would be a Safari-15.4
problem if it appeared).

The nearest analogues, for completeness:

- **`use-toast.ts` module-level mutable state.** `memoryState` (`:130`), the `listeners` array
  (`:128`) and the `toastTimeouts` map (`:57`) are shared across all 12 importing modules.
  `dispatch` (`:132`) mutates `memoryState` then synchronously calls every listener. Single-
  threaded, so no race — but it is a global store with no batching, and the `useEffect` at
  `:173-181` re-registers on every state change (dependency is `[state]`, should be `[]`).
- **`classicWorkers()`'s `bundled` Map** (`vite.config.ts:27`). Two concurrent requests for the
  same uncached worker both miss the cache and both start a full nested build; the second write
  wins. Dev-only, harmless, but it is an unsynchronised read-modify-write.
- **`src/lib/utils.ts:23` `window.commands = commands`** — a module-scope global write. Outside
  this territory (already filed by another agent as
  `minor-ui-window-commands-global-side-effect`), noted here because `src/components/ui/input.tsx`
  and every `cn()` call transitively import it, so it executes very early.

No guard can live across an `await` and the `if let` scrutinee temporary-lifetime pattern has no
JavaScript analogue.

---

## 5. Buffers and copies

No frame, audio or packet buffers exist in this territory. The relevant transfers are asset and
bundle bytes at startup, measured from `dist/`:

| Artefact | Bytes | When | Classification |
|---|---|---|---|
| `main-BYmm2ugm.js` | 914,493 | eager, render-blocking-ish (module, deferred) | parse + evaluate on the main thread |
| `main-C0KfMe13.css` | 52,395 | eager, render-blocking `<link>` | parse + style recalc |
| `Geist_wght_-AwEi7ETv.woff2` | 62,628 | after CSS parse (discovered late; no preload) | network + font decode, then a swap reflow |
| `logo-0liObS7h.svg` | 765 | favicon, low priority | — |
| `config-json-editor-BlNlvk9y.js` | 32,614 | lazy (settings) | — |
| `monaco-setup-CtMS3_Ek.js` | 2,957,044 | lazy (settings) | the single largest artefact |
| `monaco-setup-BQDoSaL3.css` | 115,735 | lazy (settings) | — |
| `monaco-editor.worker-ByQk1jEG.js` | 237,290 | lazy, worker thread | — |
| `monaco-json.worker-UHvWpAyk.js` | 369,811 | lazy, worker thread | — |
| `codicon-DCmgc-ay.ttf` | 80,340 | lazy (monaco CSS) | — |
| **total `dist/`** | **~4.7 MB** | | |

Per-render allocations worth noting (all CPU, all small, all on the main thread):

- **`cn()` on every render of every kit component.** `src/lib/utils.ts:33-35` is
  `twMerge(clsx(inputs))`. `clsx` allocates a joined string; `tailwind-merge` then tokenises it,
  builds a conflict map and allocates the result. It runs unmemoised in every `forwardRef` body
  in the kit. `tailwind-merge` v3 keeps an LRU cache keyed on the input string, so repeat renders
  with identical classes are cheap — but every component that interpolates a state-dependent
  class (e.g. `sidebar.tsx:31-33`'s `variant` flip, `input-otp.tsx:45`'s `isActive`) misses.
- **`alert-dialog.tsx:13-24` `addChildTopMargins`** clones every child of every
  `AlertDialogContent` on each render (`React.Children.map` + `cloneElement`), allocating a new
  element per child.
- **`use-toast.ts` reducer** copies the toasts array on every action (`:80,86,104,123`). With
  `TOAST_LIMIT = 1` these are one-element arrays.

No zero-copy path, no GPU involvement, no readback.

---

## 6. Queues

One, plus one dev-only cache:

- **`use-toast.ts`'s toast queue.** `memoryState.toasts` (`:130`), an unbounded array capped at
  render time by `TOAST_LIMIT = 1` (`:9`) inside `ADD_TOAST` (`:80`,
  `[action.toast, ...state.toasts].slice(0, TOAST_LIMIT)`). Non-blocking, no back-pressure —
  a burst of toasts silently discards all but the newest. The *dismissal* side is the one with
  a depth problem: `toastTimeouts` (`:57`) holds one `setTimeout` handle per dismissed toast for
  `TOAST_REMOVE_DELAY = 1000000` ms (≈16 min 40 s, `:10`) before `REMOVE_TOAST` frees the entry.
  In a long-lived host session that map only grows. Twelve modules push into this queue
  (`src/App.tsx`, the four pages, five components, `device-details.tsx`, `toaster.tsx`).
- **`classicWorkers()`'s `bundled` Map** (`vite.config.ts:27`) — unbounded (2 entries max in
  practice), never invalidated, dev-only.

`listeners` (`use-toast.ts:128`) is a fan-out list, not a queue: `dispatch` calls every entry
synchronously, so one `toast()` triggers a `setState` in all 12 consumers.

---

## 7. Syscall / FFI boundaries

None in the OS sense. The boundaries this territory crosses are:

1. **Browser → Tauri IPC.** Not called from any file in this territory directly. The kit is
   pure presentation; `src/layout/layout.tsx` reads already-fetched values out of
   `GlobalProviderContext`. The one adjacent crossing is `src/lib/utils.ts:23`
   (`window.commands = commands`), which executes at import time because `cn()` lives in the
   same module — so importing *any* kit component pulls in and runs the bindings module.
2. **`new Worker()`** (`src/lib/monaco-setup.ts:36,38`) — spawns an OS thread through the
   webview. Version-sensitive: `{type:"module"}` workers are Safari 15+, hence the whole
   `classicWorkers()` apparatus. See §14.
3. **Web font fetch** — `@font-face src: url(/assets/Geist…woff2)` over Tauri's asset protocol.
4. **`window.matchMedia`** (`index.html:11`, `src/components/theme-provider.tsx:43`) — reads the
   OS appearance setting through the WebView. `MediaQueryList.addEventListener` is Safari 14, so
   `src/lib/legacy-webview-polyfills.ts:45-73` bridges it to the deprecated
   `addListener`/`removeListener` and monkey-patches `window.matchMedia` itself.
5. **`crypto.getRandomValues`** (`src/lib/utils.ts:72,86`) — Safari 5+, safe on the floor.

No dyld/link-time exposure applies (that is a Rust/macOS concern; nothing here compiles to
native).

---

## 8. Panics

JavaScript has no panic, so this section covers unguarded throws a runtime condition can reach.

1. **`src/components/ui/input-otp.tsx:38`** —
   `const { char, hasFakeCaret, isActive } = inputOTPContext.slots[index]`. Two reachable
   `TypeError`s: rendering `<InputOTPSlot>` outside an `<InputOTP>` (the default `OTPInputContext`
   value has no `slots`), or an `index >= maxLength`. Either takes down the whole Settings page
   (`src/pages/settings.tsx:18` is the only call site) rather than degrading one field.
2. **`src/components/ui/form.tsx:45`** — `const { getFieldState, formState } = useFormContext()`.
   `useFormContext()` returns `null` outside a `<Form>`, so this destructure throws before the
   `if (!fieldContext)` guard at `:49` can ever run. That guard is itself dead: `FormFieldContext`
   defaults to `{} as FormFieldContextValue` (`:25-27`), never null. `:53`'s
   `const { id } = itemContext` is unchecked for the same reason. **Unreachable today** —
   `form.tsx` has zero importers.
3. **`src/components/ui/carousel.tsx:37`** — `useCarousel()` throws
   `"useCarousel must be used within a <Carousel />"` by design. Reachable only by
   misuse; the message is good.
4. **`vite.config.ts:12`** — `readFileSync(resolve(__dirname, file), "utf8")` with no
   `existsSync`/try-catch. Renaming `src/lib/monaco-setup.ts` or
   `src/components/json-editor.tsx` makes every Vite command die with a bare `ENOENT` from
   inside the config, before any project code loads.
5. **`vite.config.ts:70-71`** — `const { output } = first as unknown as {…}` then
   `output[0]?.code ?? ""`. The cast is unchecked; if Rollup's shape changes, `output` is
   `undefined` and `output[0]` throws inside the dev middleware. The `?? ""` also means a build
   producing no output silently serves an **empty worker script** rather than erroring.
6. **`scripts/check-legacy-webkit.mjs:56`** — `readdirSync(dir)` with no existence check. Running
   the checker before a build throws `ENOENT` rather than saying "run `pnpm build` first".
7. **`src/components/ui/carousel.tsx:3,5,7`** — three `@ts-expect-error type-err` directives on
   `import type` lines. If a future `embla-carousel-react` release fixes those types, the
   directives become *unused*, which `tsc` reports as an error — so a dependency bump can break
   `pnpm build` (which is `tsc && vite build`, `package.json:12`). Note `embla-carousel-react` is
   pinned to exactly `8.5.2` (`package.json:46`) with no caret — the only exact pin in the
   dependency list, which is consistent with someone having already fought this.

`src/layout/` and `index.html` contain no unguarded dereference; the `index.html` error handler
is itself wrapped appropriately and the theme bootstrap is inside a `try/catch` (`:10-15`).

---

## 9. Silent fallbacks and swallowed errors

1. **`vite.config.ts:70-71`** — `output[0]?.code ?? ""`. A worker build that produces no output
   is served as an empty script. The `?? ""` converts a build failure into a silently broken
   worker (the JSON editor simply has no language services). Dev-only.
2. **`index.html:10-15`** — `catch (e) { document.documentElement.classList.add("light"); }`.
   If `matchMedia` throws, the app silently falls back to light mode with no record.
3. **`scripts/check-legacy-webkit.mjs:68`** — `if (!check.hard && GUARD.test(around)) continue;`.
   This is the most consequential swallow in the territory: a genuine hazard whose ±100-char
   neighbourhood happens to contain `&&`, `||`, `?.`, `typeof ` or `catch` is **dropped from the
   report entirely** — not downgraded, not counted, not printed. In minified output those tokens
   are everywhere. The checker prints `bare/total` for rows that survive, so a fully suppressed
   check is indistinguishable from an absent one.
4. **`scripts/check-legacy-webkit.mjs:78`** — `if (!row.cover) failed++`. Any check annotated
   `cover: "shim"` can never fail, and nothing verifies the shim still exists in
   `src/lib/legacy-webview-polyfills.ts`.
5. **Tailwind purging unknown classes.** `src/components/ui/tooltip.tsx:59`'s `top-1/2!` (×4) and
   `z-60` generate no CSS under Tailwind 3 and produce no warning. Confirmed absent from
   `dist/assets/main-C0KfMe13.css`.
6. **CSS features above the floor.** `:where()` (4 preflight rules), `:focus-visible` (7 rules),
   `:has()` (1), `aspect-ratio` (1) and `inset-inline-start` (1) are all dropped by Safari 13.1's
   parser with no console message. §12 and the findings cover these.
7. **`src/components/ui/carousel.tsx:116-118`** — the effect cleanup unbinds only `select`, never
   the `reInit` handler bound at `:113`. On unmount/remount the `reInit` listener accumulates.
8. **`src/components/ui/sheet.tsx:51`** — the `trapFocus` prop is declared, never destructured,
   and therefore forwarded through `{...props}` onto the DOM, where React warns about an
   unrecognised attribute. The prop's apparent contract does nothing.
9. **`src/layout/sidebar.tsx:22-26`** — `<ChevronLeft id="hideArrow" className="lg:hidden
   cursor-pointer">` has `cursor-pointer` but **no `onClick`**. It looks interactive and is not.
   (`lg:hidden` also means it is never visible: the Tauri window's minimum width is 1050 px per
   `CLAUDE.md`, above Tailwind's `lg` 1024 px breakpoint.)

---

## 10. TODO / FIXME / HACK / XXX

A grep for `TODO|FIXME|HACK|XXX` across the entire territory (`src/components/ui/`,
`src/layout/`, `src/assets/`, `index.html`, all six config files, `components.json`,
`scripts/`, `src/index.css`) returns **zero matches**.

The only comments in the kit are:

```
src/components/ui/carousel.tsx:3:  // @ts-expect-error type-err
src/components/ui/carousel.tsx:5:  // @ts-expect-error type-err
src/components/ui/carousel.tsx:7:  // @ts-expect-error type-err
src/components/ui/use-toast.ts:1:// Inspired by react-hot-toast library
```

`vite.config.ts` carries the four numbered Tauri-template comments (`:83`, `:109-113`, `:126`)
and nothing explaining `classicWorkers()` — the single least obvious thing in the file.
`scripts/check-legacy-webkit.mjs` documents two of its 38 checks with a `why` field
(`Intl.Segmenter` at `:17`, `RegExp d/v flag` at `:44`) and none of the rest.

---

## 11. Latency path

Nothing here is on the glass-to-glass, input or audio path. A frame goes
capture → encode → RTP → client; the desktop UI never sees it. The two ways this territory
affects the engagement goal:

**A. Cold start (host app launch → interactive control UI).** Serialisation points, in order:

| Stage | Cost | Serialisation |
|---|---|---|
| WebView creates the document, runs `index.html`'s 3 inline scripts | µs | blocks parser (classic scripts) |
| Fetch + parse `main-C0KfMe13.css` (52 KB) | ~ms | **render-blocking**; no paint before it lands |
| Fetch + parse + evaluate `main-BYmm2ugm.js` (914 KB, minified) | **dominant** | React cannot mount until it finishes; unmeasured on the 10.15 Intel floor |
| Font discovered from CSS, fetched (62 KB), swapped | one extra RTT | `font-display: swap` → paint in fallback, then **reflow** |
| React mounts, `ThemeProvider` reads the store over Tauri IPC | IPC RTT | **theme flash**: the inline script guessed from the OS; the stored choice arrives later |
| `GlobalProvider` fetches session id / OTP / adapters over IPC | IPC RTT | footer and QR render empty first |

The 914 KB eager chunk is the biggest single lever, and most of it need not be there: the
walkthrough (`nextstepjs` + its `motion` peer), the carousel (`embla`), `formik` and `input-otp`
are all reachable only from routes or states that are not the first paint, yet
`createMemoryRouter` imports every route statically and there is no `manualChunks`.

**B. Main-thread contention while streaming.** The host process is simultaneously capturing,
scaling and encoding. Every main-thread stall in the webview competes with the Tauri IPC that
carries `DeviceJoin`/`DeviceModify`/`LogLine` events, so UI jank shows up as delayed device
state. The two known contributors here:

1. `src/index.css:96` — `button, input { transition: all 200ms ease-in-out !important }`. `all`
   includes layout properties, and `!important` beats every `transition-colors`/`transition-none`
   utility in the kit — including Radix Toast's `data-[swipe=move]:transition-none`
   (`toast.tsx:26`), which exists precisely to make a swipe track the finger without a transition.
2. `src/layout/layout.tsx:20-21` + `src/layout/sidebar.tsx:15` — the sidebar collapse transitions
   `width` 275 px ↔ 0 over 300 ms on the outer element while a nested `transition-all
   duration-500` runs on the inner one. `width` forces layout of the sidebar subtree *and* the
   sibling content pane every frame. There is **no `will-change` anywhere in `src/`**.

Everything else in the kit animates correctly: `switch.tsx:20` uses `transition-transform` +
`translate-x-5`; the Radix enter/exit animations come from `tailwindcss-animate`, whose `enter`/
`exit` keyframes drive only `opacity` and `transform` (via `--tw-enter-*` custom properties);
`src/pages/dashboard.tsx:290,294` animate `scale`/`opacity`. The layout-property offenders are
`layout.tsx:20`, `sidebar.tsx:15`, `bootstrap.tsx:342` and `walkthrough.tsx:267` (both
`transition-all` on what appear to be progress indicators — likely `width`, **unverified**), plus
the global `!important` rule.

Cost estimates per stage are deliberately absent: none was measured on the macOS 10.15 dev box,
and guessing them would be worse than saying so.

---

## 12. Stale references

**`CLAUDE.md` vs. the code — three disagreements** (per the brief, the code wins):

1. **Version.** `CLAUDE.md` § "Versioning & releases" says *"Current version: **0.5.4**"*.
   `package.json:4` says `0.6.3`, matching `git log` (`4b745d1 build(release): bump version to
   0.6.3`). The doc is three releases stale.
2. **`pnpm approve-builds`.** `CLAUDE.md` § "Commands" lists it as an interactive step.
   Commit `0f745d8` ("ops: drop the pnpm approve-builds step from CI and setup docs") removed it
   from CI; `pnpm-workspace.yaml` carries `allowBuilds`. `CLAUDE.md` still documents it (with a
   caveat), so it is half-updated.
3. **What the legacy checker covers.** `CLAUDE.md` says
   *"`node scripts/check-legacy-webkit.mjs` re-derives the list from `dist/` after a build and
   fails on anything unpatched."* The script reads `dist/assets/*.js` only — never CSS, never
   `dist/index.html`, never subdirectories — and cannot fail on a check that carries a `cover`
   annotation. The shipped CSS contains five distinct post-13.1 features today and the checker
   exits 0. Treating it as a general "will this load on the floor?" gate is the actual risk here.

**Documents referenced but absent:** none in this territory. `components.json:2` points at
`https://ui.shadcn.com/schema.json` (external, live). No `§`, `M1`/`M2` or `PRD` references, no
AI-tooling references, in any file I read.

**Comments that no longer describe the code:**
- `vite.config.ts:83` `// https://vitejs.dev/config/` and `:109-113` (the numbered Tauri-template
  comments) are boilerplate that survived; `:111` *"prevent vite from obscuring rust errors"*
  still describes `clearScreen: false` correctly.
- `src/components/ui/use-toast.ts:1` *"Inspired by react-hot-toast library"* — accurate, but the
  file has since diverged from upstream shadcn only in that it was **not** updated (it still
  carries the upstream `TOAST_REMOVE_DELAY = 1000000` placeholder).
- `src/components/ui/carousel.tsx:3,5,7` *"type-err"* — three suppressions with no explanation of
  *which* type error, on a dependency pinned to an exact version. The next person to bump
  `embla-carousel-react` has nothing to go on.

**Stale code rather than stale comments:**
- `src/layout/sidebar.tsx:15,24` — `lg:` responsive classes (`lg:border-r-0`, `lg:relative`,
  `lg:block`, `lg:hidden`) in an app whose window minimum width is 1050 px, above Tailwind's `lg`
  breakpoint (1024 px). The `lg:` branch is always taken; the mobile branch, including the
  `hideArrow` chevron, is unreachable.
- `src/components/ui/input.tsx:25` — `rtl:peer-focus:left-auto` and
  `rtl:peer-focus:translate-x-1/4` in an app with one locale (`src/i18n/locales/en.json`).
- `tailwind.config.js:5-7` — `./pages/**`, `./components/**`, `./app/**` are Next.js-scaffold
  leftovers matching nothing at the repo root.

---

## 13. Dead code candidates

| Candidate | Evidence |
|---|---|
| `src/components/ui/form.tsx` | Zero importers. Searched `src/**/*.{ts,tsx}` for `components/ui/form"` → 0 hits. It is also the sole importer of `react-hook-form`. |
| `src/components/ui/resizable.tsx` | Zero importers (same search method). Sole importer of `react-resizable-panels` **and** of `@radix-ui/react-icons`. |
| `zod` ^3.24.2 | Zero imports anywhere in `src/`. |
| `@hookform/resolvers` ^4.1.0 | Zero imports. (Its only purpose is bridging `zod` to `react-hook-form`, both of which are also unreachable.) |
| `react-avatar` ^5.0.3 | Zero imports. `src/components/ui/avatar.tsx:2` uses `@radix-ui/react-avatar`. |
| `react-svg` ^16.3.0 | Zero imports. |
| `@radix-ui/react-tooltip` ^1.1.8 | Zero imports — superseded by `@base-ui/react` at `src/components/ui/tooltip.tsx:3`, but never removed. |
| `@tauri-apps/plugin-dialog` ~2.3.0 | Zero JS imports. The Rust `tauri-plugin-dialog` crate is what `lib.rs` registers; the npm package is the JS binding and nothing calls it. |
| `src/components/ui/sheet.tsx:51` `trapFocus?: boolean` | Declared, never destructured, forwarded to the DOM. |
| `src/layout/sidebar.tsx:22-26` `#hideArrow` | `cursor-pointer`, no handler, and `lg:hidden` in a window that is always ≥ `lg`. |
| `src/components/ui/tooltip.tsx:59` `top-1/2!` ×4, `z-60` | Tailwind v4 syntax / off-scale value under Tailwind v3; generate no CSS. Verified absent from the built stylesheet. |
| `tailwind.config.js:5-7` three globs | Match nothing. |
| `src/index.css:133-135` `[data-name="nextstep-pointer"]` | Live — targets `nextstepjs`'s injected element. **Not dead**; noted because it is the only rule outside `@layer base` and looks orphaned. |

**Explicitly not dead, despite appearances:** `motion` ^13.1.0 has no direct import in `src/`, but
`node_modules/nextstepjs/package.json` declares `"motion": ">=11"` as a peer dependency and
framer-motion internals are demonstrably in the bundle
(`Object.hasOwnProperty.call(Element.prototype,"animate")`, `motionVal…` in
`dist/assets/main-BYmm2ugm.js`). Removing it breaks the walkthrough. Also `formik` ^2.4.6 — one
importer (`src/components/pages/device-details.tsx:51`), so the repo genuinely carries two form
libraries, one of which serves only a dead component.

---

## 14. Capability probing

This is where the territory has its real defects. The project's rule (per `CLAUDE.md`, stated for
the macOS backends but applied here too) is **probe the capability, never the version number**.
The frontend's version-vs-capability story is mixed.

**Done right:**
- `src/lib/legacy-webview-polyfills.ts` probes throughout: `define()` (`:75-79`) installs a shim
  only `if (typeof holder[name] !== "function")`; `bridgeMediaQueryListEvents` (`:45-55`) checks
  `typeof list.addEventListener === "function"` before bridging;
  `shimRegExpFlags` (`:131-141`) determines which flags to strip by **actually constructing**
  `new Native("", flag)` in a try/catch rather than testing a version. That is exactly the
  doctrine.
- `dist/assets/main-BYmm2ugm.js`'s two floating-ui `element.matches(":popover-open")` /
  `":modal"` call sites are both `try{}catch{}`-guarded by the library itself.
- Monaco's own `crypto.randomUUID` and `requestIdleCallback` uses are `typeof`-guarded, and its
  lookbehind regex `new Gl("(?<=\\.)","gm")` is a lazy wrapper with an internal
  `try{new RegExp(...)}catch{}` — so a Safari-13.1 `SyntaxError` there is absorbed rather than
  taking down the 2.9 MB chunk. We are protected by a dependency's defensiveness, not by anything
  we own.

**Done by version number, or not at all:**
- **`esbuild.target: "safari13"`** (`vite.config.ts:96,99`) and
  `build.target: [..., "safari13"]` (`:88`) are a hardcoded version floor. That is correct and
  unavoidable for a *syntax* target — you cannot probe a parser. Worth noting the number
  disagrees with `package.json:6` `browserslist: ["safari >= 13.1", …]`: esbuild's `safari13`
  means 13.0, which lacks `?.`/`??` (13.1 features), so esbuild lowers slightly more than
  necessary. Errs safe; the discrepancy is undocumented and will confuse the next reader.
- **`scripts/check-legacy-webkit.mjs` encodes a version table (`since`) with no probe and no
  runtime consequence.** It is a static-analysis gate, which is the right shape — but it has four
  structural gaps, each of which is a place where a capability is neither probed nor checked:
  1. **CSS is never examined** (`:56` filters `.js`). The shipped stylesheet currently contains
     `:focus-visible` (Safari 15.4, 7 rules), `:where()` (14.0, 4 preflight rules), `:has()`
     (15.4, 1), `aspect-ratio` (15.0, 1) and `inset-inline-start` (14.1, 1). All are dropped
     silently on the floor. The `:focus-visible` case is the worst — combined with
     `src/index.css:97`'s `outline: 2px solid transparent`, **the app has no visible keyboard
     focus indicator at all on macOS 10.15**.
  2. **`dist/index.html` is never examined**, so `index.html`'s three inline classic scripts
     (which esbuild does not transform) are outside the gate entirely.
  3. **`readdirSync` is non-recursive**, so any subdirectory Vite emits is skipped without a word.
  4. **`cover` annotations are unverified claims** (`:78`). Delete `shimReplaceChildren` from
     `legacy-webview-polyfills.ts` and the checker still prints `replaceChildren 5/6 shim` and
     exits 0.
  Plus the `GUARD` heuristic (`:51`, a ±100-char window against
  `/typeof |\?\.|&&|\|\||catch/`) which, on minified code, drops genuine hits silently — see §9.
- **Missing checks for capabilities the codebase is actively moving toward:**
  - `inert` (Safari 15.5) is **not** in `CHECKS`. `@base-ui/react`'s
    `floating-ui-react/utils/markOthers.js` uses it, and that module is shared internals for every
    Base UI component doing focus management. `src/components/ui/tooltip.tsx:3` shows the
    Radix → Base UI migration has begun; the current bundle is clean (0 DOM-`inert` hits — the 4
    `.inert` matches in `main-*.js` are `react-remove-scroll`'s own prop of that name), so this is
    a next-component hazard, not a live break.
  - **RegExp lookbehind** `(?<=` / `(?<!` (Safari 16.4) is not in `CHECKS`, and esbuild does not
    lower it. In a regex *literal* it is a parse-time `SyntaxError` that kills the entire chunk
    before a line of it runs — the exact failure class the checker exists to prevent.
  - **Module workers** `new Worker(url, {type:"module"})` (Safari 15) are not checked, and
    `worker.format` is not pinned in `vite.config.ts`. Production correctness rests entirely on
    Vite 6's `"iife"` default. This is the one place where a *silent* dependency-bump regression
    would take out a whole feature (the settings JSON editor) on one platform only.
- **`index.html:11`** picks the theme from `prefers-color-scheme` alone and never reads the
  persisted choice, so a user whose stored theme disagrees with the OS sees a flash. That is a
  data-availability problem rather than a capability-probing one, but it lives in the same
  bootstrap script.

**Missing tier:** there is no "this browser is below the floor" tier beyond the boot-failure panel
at `index.html:34-52`, which only fires on an uncaught `error` event. It does not listen for
`unhandledrejection`, so a rejected promise during startup shows a blank window with no
explanation. Given the panel's text explicitly names Safari 13.1, adding an
`unhandledrejection` listener is cheap and in keeping with its intent.

---

## 15. Questions and cross-cutting concerns

1. **Is Safari 13.1 still the floor, and does Base UI change the answer?** `tooltip.tsx` migrated
   one component from Radix to `@base-ui/react` 1.6.0, whose shared internals use `inert` (15.5),
   `checkVisibility` (17.4), `CSS.registerProperty` (16.4), `Object.hasOwn` (15.4) and
   `findLast` (15.4). Tree-shaking keeps all of it out today. Continuing the migration
   component-by-component will bring `inert` in with the first Dialog or Menu, and nothing checks
   for it. This needs a decision, not a discovery. **Cross-cutting**: it also duplicates
   floating-ui in the eager bundle (one copy inside Radix, one inside Base UI).

2. **Who owns the legacy checker's scope?** It is the only automated compatibility gate in the
   repo, CI is disabled, and it does not cover CSS — where five live regressions already exist.
   Extending it is cheap and high-leverage, but it spans this territory and the client web page
   (`src-tauri/src/streamer/static/*.js`, which has an even older compatibility contract and is
   currently checked by nothing at all). Recommend one owner for both.

3. **Startup budget.** Is there a target for host-app launch time? 914 KB eager JS + 52 KB
   render-blocking CSS + a late-discovered 62 KB font is a lot for a control panel, and route
   splitting is nearly free given `createMemoryRouter` (no URLs to preserve). I have byte counts
   but no wall-clock measurement on the macOS 10.15 Intel dev box — someone with that machine
   should measure before and after, since it is the constraint that justifies the work.

4. **`src/index.css:96`'s `transition: all !important` crosses into other territories.** It
   silently overrides transition decisions in every component anyone else is looking at
   (`src/pages/*`, `src/components/*`), and the Monaco counter-patch at `:116-118` is evidence it
   has already caused one visible regression. Whoever removes it should re-verify Monaco input
   behaviour and the Radix toast swipe together.

5. **Two form libraries.** `formik` (one live consumer) and `react-hook-form` (only the dead
   `form.tsx`). Consolidating is a small cleanup but touches
   `src/components/pages/device-details.tsx`, which is another agent's territory.

6. **`src/lib/utils.ts:23` `window.commands = commands`** executes at import time and every kit
   component pulls it in transitively via `cn()`. It is outside my territory (already filed as
   `minor-ui-window-commands-global-side-effect`), but the coupling is worth flagging: there is no
   way to import a single UI component without also publishing the entire Tauri command surface
   on `window`.

7. **`embla-carousel-react` is pinned to exactly `8.5.2`** — the only exact pin among 47
   dependencies — with three `@ts-expect-error type-err` suppressions and no note explaining
   either. If those types are ever fixed upstream, the unused directives become `tsc` errors and
   `pnpm build` fails. Someone should write down what the original type error was.

---

## Findings written

18 files in `findings/`:

| File | Severity |
|---|---|
| `major-build-single-eager-startup-chunk.md` | major |
| `major-ui-global-transition-all-important.md` | major |
| `major-build-legacy-webkit-checker-blind-spots.md` | major |
| `major-ui-focus-visible-dropped-on-safari-13.md` | major |
| `major-build-vite-worker-format-unpinned.md` | major |
| `minor-ui-tooltip-tailwind-v4-syntax-dead-classes.md` | minor |
| `minor-ui-sidebar-animates-layout-properties.md` | minor |
| `minor-ui-input-silently-drops-forwarded-ref.md` | minor |
| `minor-ui-dead-components-and-unused-dependencies.md` | minor |
| `minor-build-startup-theme-and-background-flash.md` | minor |
| `minor-ui-font-fallback-is-serif-and-unpreloaded.md` | minor |
| `minor-ui-tailwind-preflight-where-rules-die-on-safari-13.md` | minor |
| `minor-ui-aspect-ratio-and-logical-inset-above-the-floor.md` | minor |
| `minor-build-index-html-blocks-modifier-clicks-globally.md` | minor |
| `minor-build-vite-config-fragile-and-underspecified.md` | minor |
| `minor-build-tailwind-content-globs-and-eslint-scope-gaps.md` | minor |
| `minor-ui-kit-upstream-defects-carried-verbatim.md` | minor |
| `speculative-ui-base-ui-migration-drags-post-floor-apis.md` | speculative |

(18 written by M2; the `:has()` case in `table.tsx` was already filed by another agent as
`minor-ui-safari13-has-selector-in-table.md` and is deliberately not duplicated.)
