# M1 — The React desktop control UI (`src/`)

Territory: `src/main.tsx`, `src/App.tsx`, `src/pages/`, every non-`ui/` file in `src/components/`,
`src/lib/`, `src/i18n/`, plus the build inputs that shape them (`vite.config.ts`, `index.html`,
`tailwind.config.js`, `package.json`).

This is the **host-side control UI** only. It never touches a video frame, an audio packet or an
input event — those belong to the client web page (`src-tauri/src/streamer/static/`) and the Rust
core. Everything here is on the *startup* and *control-plane* paths.

---

## 1. Files and purpose

### Entry and shell

**`src/main.tsx`** (15 lines). The entry module. Imports `@/lib/legacy-webview-polyfills` first —
before React — so the shims are installed before any dependency evaluates, then
`ReactDOM.createRoot(document.getElementById("root")!).render(<><App /><Toaster /></>)`. No
`StrictMode`, no error boundary, no `Suspense` at the root. `index.css` is imported here, so the
Tailwind bundle is render-blocking.

**`src/App.tsx`** (417 lines). The application root and the single owner of all global state. Holds
11 `useState` slices, publishes them through `GlobalProviderContext` as `[value, setter]` tuples,
registers eight backend event listeners, and wraps everything in
`ThemeProvider → NextStepProvider → NextStepReact → RouterProvider`. The router is a
`createMemoryRouter` built **at module scope** (`:49-58`), so all four route components are
statically imported and the route tree is constructed before `App` ever renders.

**`src/layout/layout.tsx`** (61 lines). The chrome around every non-bootstrap route: a collapsible
275 px sidebar (width animated via inline style), a header with the sidebar toggle and
`ProfileMenu`, a scroll container for `children`, and a footer showing the session ID and OTP.
Reads `windowOtp` and `windowSessionId` from the global context.

**`src/layout/sidebar.tsx`** (67 lines). Three `<Link>`-wrapped buttons whose `variant` is derived
from `useLocation().pathname`. Purely presentational.

### Routes

**`src/pages/bootstrap.tsx`** (639 lines). The startup state machine, mounted at `/`. Runs
`start() → runUpdate() → proceed() → ensurePermissions() → checkSystemRequirements() → runSetup()`,
then navigates to `/dashboard` by programmatically clicking a hidden `<Link id="dashlink">`.
Renders a spinner plus five `AlertDialog`s (update progress, update failure, setup error,
compatibility report, permission gate). Also kicks off the monaco preload.

**`src/pages/dashboard.tsx`** (365 lines). The join screen. Renders one `QrDisplay` per network
adapter plus one for the cloud relay, in two parallel trees (a `lg:flex` row and an `embla` carousel
for narrow windows). Subscribes to `cloudStatusChange`, fetches `get_display_capacity`, and
auto-starts the walkthrough when `walkthroughCompleted` is false. Gates its whole render on
`statusLoaded` and on `capacity?.full`.

**`src/pages/devices.tsx`** (194 lines). A table of connected devices, one row per entry in
`windowDevices`, each with a `RefreshStream` button and a `DeviceDetails` editor. Also opens the OS
display-settings panel via `@tauri-apps/plugin-shell`.

**`src/pages/settings.tsx`** (1600 lines). The largest file in the frontend. One component with 34
`useState` hooks, 11 controlled text inputs, 7 cards, an "Advanced" disclosure containing the TURN
config, server ports, the GPU-encode toggle, the lazily loaded config JSON editor and the log
terminal, plus four `AlertDialog`s and the avatar crop modal. Uses `useBlocker` from react-router to
intercept navigation while the JSON editor is dirty.

### Components (non-`ui/`)

**`src/components/global-provider.tsx`** (49 lines). Type definitions and the `createContext` call
for `GlobalProviderContext`. Exports `GlobalContextType` (11 `[value, setter]` tuples),
`AudioOutputsInfo`, and a `GlobalContextDefault` object that is declared but never imported
anywhere. No provider component — `App.tsx` renders `.Provider` directly with an inline object.

**`src/components/config-provider.tsx`** (235 lines). The config store façade. Opens
`tauri-plugin-store`'s `config.json` once at module scope, and exposes `getConfig`, `updateConfig`,
`flushConfig`, `createConfig`, a `useSyncExternalStore`-backed `useConfig()` hook, plus device
helpers (`getSavedDevices`, `saveDeviceSettings`, `removeSavedDevice`, `getKnownDevices`,
`recordKnownDevice`, `setKnownDeviceBanned`, `removeKnownDevice`). Owns the `Config`, `Device` and
`KnownDevice` types and `defaultConfig`.

**`src/components/theme-provider.tsx`** (115 lines). Applies `light`/`dark` to
`documentElement.classList`, resolving `"system"` through `matchMedia`. Listens to Tauri's
`onThemeChanged` and re-resolves when the stored theme is `"system"`. `setTheme` is async because it
persists through `updateConfig`.

**`src/components/mode-toggle.tsx`** (45 lines). A three-item dropdown calling `setTheme`.

**`src/components/profile-menu.tsx`** (175 lines). The avatar dropdown: Exit App, Reset Preferences,
Replay Tour, Uninstall Drivers. Reads `windowClosing`/`windowAvatar` from context and `name` from
`useConfig()`.

**`src/components/walkthrough.tsx`** (287 lines). The onboarding tour: six step definitions, a
`WalkthroughCard`, a `WalkthroughArrow`, and `HighlightProxy` — a fixed-position `<div>` that tracks
the current step's target element via a `requestAnimationFrame` loop, so `nextstepjs` can point at a
single stable selector. Also re-enters the tour on window resize and zoom change.

**`src/components/log-terminal.tsx`** (110 lines). Fetches `get_log_backlog`, subscribes to
`logLine`, keeps the last 2000 lines in a `useState<string[]>`, renders one `<div>` per line, and
auto-scrolls when pinned to the bottom. Mounted only inside Settings > Advanced.

**`src/components/config-json-editor.tsx`** (588 lines). The `config.json` editor: loads and
serializes the config, wraps `JsonEditor`, tracks dirty state, validates on save via
`validateConfig`, applies via `applyConfig`, and handles the concurrent-edit conflict case by
showing two read-only `ComparePane` editors side by side. Watches `documentElement`'s class with a
`MutationObserver` to follow the theme.

**`src/components/json-editor.tsx`** (443 lines). The monaco wrapper. Owns the schema registry,
value masking (`-webkit-text-security: disc` decorations over secret paths), custom fold-tail
decorations, debounced JSON worker validation (900 ms idle), and a `useImperativeHandle` surface
(`format`, `minify`, `setValue`, `revealLine`, `focus`, `getValue`, `remeasureFonts`).
`React.memo`'d.

**`src/components/avatar-crop-modal.tsx`** (222 lines). A canvas-based 256x256 square cropper with
pointer drag and a zoom slider, exporting a PNG blob. Self-rendered modal (`if (!open) return null`),
not a Radix dialog.

**`src/components/pages/device-details.tsx`** (818 lines). The per-device editor `<Sheet>`: name,
orientation, remote control, system audio (with the macOS driver-install prompt and audio-output
`<Select>`), scale, pixel ratio, refresh rate, video scale and video quality. Backed by `useFormik`.
Also exports `DeleteDevice`.

### `src/lib/`

**`bindings.ts`** (269 lines, **generated by tauri-specta — never hand-edit**). 43 commands and 12
events. Notable shapes: `set_window_zoom` is the only command returning `Result<null, string>`;
`get_avatar`/`set_avatar` cross as `number[]`; `getDisplayCapacity` returns
`{max: number|null, in_use, full, backend}`.

**`legacy-webview-polyfills.ts`** (212 lines). Seven shims for the Safari 13.1 floor:
`MediaQueryList.addEventListener` bridging, `WeakRef`, `AggregateError`, `replaceChildren`,
`Array/String.prototype.at`, `findLast`/`findLastIndex`, and a `RegExp` `d`/`v` flag stripper that
reconstructs `match.indices` by hand.

**`monaco-setup.ts`** (217 lines). Imports 24 individual monaco ESM contributions rather than
`editor.all.js`, installs `window.MonacoEnvironment`, defines two custom themes, disables most of the
JSON mode's built-in features (folding, colors, diagnostics, symbols, selection ranges) in favour of
its own `scanJson`-backed folding provider, and exports the shared `editorOptions`.

**`monaco-editor.worker.ts` / `monaco-json.worker.ts`** (2 lines each). Worker entrypoints; both
import the polyfills first.

**`preload-editor.ts`** (41 lines). Warms the editor: dynamic-imports the config editor and monaco,
spawns the JSON worker, creates and disposes a throwaway offscreen editor to prime monaco's global
font-measurement cache.

**`json-structure.ts`** (123 lines). A hand-written single-pass JSON scanner producing bracket pairs
(for folding) and string-value positions with dotted paths (for masking). Memoizes its last result.

**`config-schema.ts`** (361 lines). The JSON Schema for `config.json` — every key documented with a
description, type, range and default, which is what drives monaco's hover and completion — plus a
hand-written `validateConfig` walker and three cross-field checks.

**`apply-config.ts`** (89 lines). Pushes an edited config into `AppState` command by command, then
persists it, wrapping each step with a path label for error attribution.

**`zoom.ts`** (37 lines). An 11-entry zoom ladder, nearest-step in/out, clamping, and `applyZoom`
(which unwraps `set_window_zoom`'s `Result`).

**`avatar.ts`** (42 lines). `loadAvatar` / `saveAvatar` / `clearAvatar` over the `number[]` bridge,
with a `FileReader`-based data-URL conversion.

**`utils.ts`** (124 lines). `cn`, `buildQrValues` (the LAN join URLs), `buildCloudQrValue`,
`buildWifiQrValue`, CSPRNG `generatePassword`/`generateOtp`, plus the `window.commands` global
assignment and three dead helpers.

**`next-navigation-stub.ts`** (17 lines). Five no-op hooks aliased over `next/navigation` in
`vite.config.ts` so `nextstepjs` loads outside Next.

### `src/i18n/`

**`index.ts`** (84 lines). A ~60-line i18n implementation: `import.meta.glob(..., {eager:true})`
over `locales/*.json`, a module-level `currentLocale` persisted in `localStorage`, a `Set` of
listeners, dotted-path `lookup`, `{name}` interpolation, and a `useSyncExternalStore`-backed
`useTranslation()`.

**`locales/en.json`** (431 lines, 285 leaf keys). The only locale. Covers toasts, dialogs, bootstrap
and the config editor — but **not** the page bodies: Dashboard, Devices and most of Settings
hardcode their English strings inline.

---

## 2. Function catalog

### `src/main.tsx`
No functions; a single top-level `createRoot(...).render(...)`.

### `src/App.tsx`
| symbol | line | purpose / notable behavior |
| --- | --- | --- |
| `stopListening` | 32 | wraps an unlisten fn, swallows rejection into `console.error` |
| `DEVICE_OVERRIDE_KEYS` | 36 | the 10 device fields `syncOverrides` reconciles against the store |
| `router` | 49 | `createMemoryRouter` built at **module scope** — four eagerly imported routes |
| `App` | 60 | the root; 11 state slices, 10 effects |
| — avatar effect | 79 | `loadAvatar()` on mount; failure sets `null` |
| — zoom-restore effect | 89 | reads `zoomFactor` from config, `applyZoom`, sets `zoomReady` |
| — zoom-apply effect | 110 | on every `zoom` change: `applyZoom` + `updateConfig`; reverts and toasts on failure |
| — zoom-keybind effect | 125 | window `keydown`, Ctrl/Cmd `+`/`-`/`0` |
| — `sessionIdRef`/`devicesRef` mirrors | 143,146 | refs so event callbacks read fresh values without re-subscribing |
| — `syncOverrides` effect | 149 | on every config publish, merges saved per-device overrides into live devices; **correctly returns `prev` when unchanged** |
| — notification-permission effect | 176 | `isPermissionGranted` then `requestPermission` |
| `notifyIfUnfocused` | 190 | sends an OS notification only when the window is unfocused |
| — session-credentials effect | 200 | `set_session_credentials` whenever `sessionId`+`otp` are both set |
| — disconnect-grace effect | 213 | replays `localStorage.disconnectGraceSecs` once `loaded` |
| — event-listener effect | 222 | registers 8 listeners: `deviceJoin`, `deviceModify`, `deviceAudioOutputs`, `deviceRemove`, `hostedNetworkNoPassword`, `sessionIdChange`, `networkChange`, `joinAttemptsPaused` |
| — close-request effect | 337 | `onCloseRequested` → `stopHostedNetwork` → `exitApp`; re-registers whenever `loaded` flips |
| `markWalkthroughDone` | 363 | persists `walkthroughCompleted` and flushes |

### `src/pages/bootstrap.tsx`
| symbol | line | purpose |
| --- | --- | --- |
| `samePermissions` | 31 | shallow key/granted comparison, used to skip no-op state updates in the poll |
| `withBold` | 37 | splits a `{var}` template and bolds the substitution |
| `formatBytes` | 48 | B/KB/MB/GB for the update progress line |
| `runSetup` | 79 | the long one: `setup()` → create-or-read config → apply theme, ports, GPU flag, volume proxy → replay every saved device and known device → read `publicSessionsEnabled` → `watch_for_network_changes` → mint session id + OTP → `set_session_credentials` → register/unregister cloud → `buildQrValues` → apply TURN → navigate. On failure, optionally installs drivers, sleeps 5 s, retries once |
| `runUpdate` | 171 | `check()`, then `downloadAndInstall` with progress, then `relaunch()` |
| `ensurePermissions` | 205 | returns `true` if any required permission was missing (which *pauses* startup); returns `false` both when nothing is missing **and** when the check threw |
| `recheckPermissions` | 225 | the manual "Re-check" button |
| `proceed` | 244 | permissions → compatibility report → blocking/limited dialog → `runSetup` |
| `start` | 274 | `runUpdate` then `proceed` |
| — mount effect | 279 | `running` ref guards double-invocation |
| — preload effect | 286 | `scheduleConfigEditorPreload()` |
| — permission-poll effect | 291 | 200 ms interval + `focus` listener while the gate is open |

### `src/pages/dashboard.tsx`
| symbol | line | purpose |
| --- | --- | --- |
| `CloudBadge` | 33 | maps `{registered, connecting, offline, error}` to a coloured pill |
| `Dashboard` | 53 | the route |
| — walkthrough effect | 62 | module-level `walkthroughAutoStarted` latch + `getConfig()` check + `requestAnimationFrame` before starting |
| — cloud-status effect | 78 | subscribes to `cloudStatusChange`, then reads `get_cloud_status`, preferring the live event; toasts if neither arrives |
| — capacity effect | 117 | `get_display_capacity` on every `devices` identity change |
| `QrDisplay` | 224 | one QR card: heading, `<QRCode size={500}>`, the URL with a copy button, and the expand modal |
| `QrModalComponent` | 305 | a `createPortal` full-screen QR with an Escape handler |

### `src/pages/devices.tsx`
| symbol | line | purpose |
| --- | --- | --- |
| `RefreshStream` | 32 | one button per row → `refresh_device_stream`, toasts restarted/offline/failed |
| `Devices` | 87 | the table; opens `control desk.cpl` (Windows) or `x-apple.systempreferences:` (macOS) |

### `src/pages/settings.tsx`
| symbol | line | purpose |
| --- | --- | --- |
| `isLikelyIp` | 66 | dotted-quad with octet range check, or a hex/colon v6 shape |
| `formatLastSeen` | 74 | "Just now" / "N min ago" / "N hr ago" / "N days ago" / a locale date |
| `Settings` | 86 | the route; 34 state hooks |
| — blocker effect | 140 | on `useBlocker` firing, either auto-proceeds (`dontShowAgain.configEditor`) or opens the confirm dialog |
| `rememberConfigDialogChoice` | 159 | persists the editor's don't-show-again |
| `handleConfigSaved` | 172 | after a JSON-editor save, re-seeds every Settings field, re-applies the theme, re-registers the cloud session and rebuilds the QR values |
| `handleNetworkNameChange` | 206 | forces the SSID to keep the `ScreenExtend` prefix, caps at 32 chars |
| — config-load effect | 225 | the initial read; also `get_disconnect_grace` |
| — autostart effect | 263 | `isAutostartEnabled()` |
| — known-devices effect | 267 | re-reads known devices on every `connectedDevices` identity change |
| `isConnected` | 282 | linear `.some()` over connected devices |
| `applyBan` / `banManualIp` / `forgetDevice` | 284,310,323 | ban/unban/forget, each pushing to both the backend and the store |
| `sortedKnownDevices` | 345 | **recomputed and re-sorted on every render**, with `isConnected` inside the comparator |
| `saveDisconnectGrace` | 350 | clamps 0-600, pushes, mirrors into `localStorage` |
| `saveTurnConfig` | 383 | trims, pushes, persists |
| `togglePublicSessions` | 407 | optimistic toggle with rollback; registers/unregisters the cloud session |
| `toggleAutostart` | 436 | enable/disable, re-reads on failure |
| `toggleGpuEncode` | 457 | optimistic, re-reads the backend value on failure |
| `saveServerPorts` | 478 | validates 1-65535 and http≠https, applies, rebuilds QR values, persists, flushes |
| — OTP spin effect | 527 | 500 ms spinner then `generateOtp()` |
| — credentials-persist effect | 537 | **writes the store on every keystroke** |
| — wifi-QR effect | 554 | closes the QR modal when the network stops |
| `startNetworkWithFeedback` | 558 | stop → start → on failure check `isWifiOn` and offer the Wi-Fi dialog |
| `onFileSelected` / `closeCrop` / `handleCropSave` / `handleRemoveAvatar` | 599-644 | the avatar flow; `URL.createObjectURL` is correctly revoked in `closeCrop` |

### `src/components/config-provider.tsx`
| symbol | line | purpose |
| --- | --- | --- |
| `sameDevice` | 34 | token match if a token exists, else IP match against a tokenless entry |
| `defaultConfig` | 72 | the 14-key shape |
| `ConfigDB` | 104 | `Store.load("config.json")` at module scope; the rejection is logged and then re-awaited by every caller |
| `publish` | 110 | sets `snapshot` and notifies **unconditionally** |
| `subscribeToConfig` / `getConfigSnapshot` | 115,120 | the external-store pair |
| `getConfig` | 122 | 1 + 14 sequential IPCs |
| `updateConfig` | 134 | one IPC per changed key; on failure re-reads to resync, then rethrows |
| `useConfig` | 151 | `useSyncExternalStore` + a lazy first load guarded by module-level `loading`/`loadFailed` |
| `flushConfig` | 166 | `db.save()` |
| `createConfig` | 171 | seeds defaults and a generated SSID/password |
| `getSavedDevices`/`saveDeviceSettings`/`removeSavedDevice` | 175-188 | device overrides; `saveDeviceSettings` deliberately blanks `name` |
| `getKnownDevices`/`recordKnownDevice`/`setKnownDeviceBanned`/`removeKnownDevice` | 190-235 | the known-device list; `recordKnownDevice` merges, never expires |

### `src/components/json-editor.tsx`
| symbol | line | purpose |
| --- | --- | --- |
| `modelUri` | 40 | `inmemory://model/<name>.json` |
| `toSeverity`/`toMarker` | 69,78 | LSP diagnostic → monaco marker (1-based conversion) |
| `isMasked` | 99 | exact path match, or with `[n]` normalized to `[]` |
| `schemasUnchanged`/`applySchemas`/`applySchemasNow` | 105-132 | a module-level schema registry so multiple editors coexist |
| `severityOf` | 134 | monaco severity → the local union |
| `JsonEditorBase` | 138 | the component |
| — schema effect | 163 | registers/unregisters this editor's schema |
| — worker keepalive effect | 173 | pings the JSON worker every 60 s |
| — marker effect | 185 | republishes markers on `onDidChangeMarkers` |
| `isCollapsed` | 209 | infers a fold by comparing `getTopForLineNumber` of adjacent lines |
| `maskedContent` | 216 | ranges for masked strings, cached on `(structure, paths)` identity |
| `maskAt` | 237 | the masked value under a position |
| `refreshDecorations` | 245 | rebuilds mask + fold-tail decorations, short-circuiting on an unchanged key string |
| `format`/`replaceAll` | 286,290 | format action; full-range edit with an undo stop |
| `handleMount` | 327 | wires 8 editor events, the 900 ms debounced validation, and a Ctrl+Shift+F action |

### `src/components/config-json-editor.tsx`
| symbol | line | purpose |
| --- | --- | --- |
| `serialize` | 63 | `JSON.stringify(config, null, 2) + "\n"` — the canonical form the whole conflict logic compares against |
| `MASKED_PATHS` | 65 | password, credential, and both token arrays |
| `describeParseError` | 75 | recovers line/column from a `JSON.parse` message, by "line N column M" or by byte position |
| `ConfigJsonEditor` | 96 | the component |
| — theme effect | 124 | `MutationObserver` on `documentElement.class` |
| — zoom effect | 132 | 150 ms debounced `remeasureFonts()` |
| `load` | 137 | reads and seeds |
| `adopt` | 169 | replaces both buffers and the editor value |
| — store-subscription effect | 178 | adopts external changes **only when the buffer is clean** |
| `problems`/`stats` | 196,209 | memoized markers+save errors, and a hand-written UTF-8 byte counter |
| `revert`/`copy`/`fail`/`commit`/`save` | 228-326 | `save` re-checks the stored snapshot and raises a conflict pane rather than clobbering |
| `resolveWithMine`/`resolveWithStored` | 328,335 | conflict resolution |
| `ComparePane`/`ToolButton` | 532,564 | read-only editor pane; tooltip icon button |

### `src/lib/` and `src/i18n/`
| symbol | file:line | purpose |
| --- | --- | --- |
| `bridgeMediaQueryListEvents` | polyfills:45 | bridges `addEventListener("change")` onto `addListener`; **patches `window.matchMedia` unconditionally** |
| `define` | polyfills:75 | the capability guard — skips when the member is already a function |
| `shimWeakRef`/`shimAggregateError`/`shimReplaceChildren`/`shimRelativeIndexing`/`shimFindLast` | polyfills:81-204 | all `define`-guarded, so no-ops on modern hosts |
| `shimRegExpFlags` | polyfills:131 | probes `d`/`v` support and **returns early when both work**; otherwise replaces global `RegExp` and rebuilds `match.indices` via `locate` |
| `preloadConfigEditor`/`run`/`scheduleConfigEditorPreload` | preload-editor:3,8,39 | the once-only warm-up; `started` promise dedupes |
| `pathOf`/`scanJson`/`scanJsonUncached` | json-structure:24,39,47 | last-result memo + a single-pass scanner tracking an object/array frame stack |
| `str`/`bool`/`int`/`configJsonSchema` | config-schema:5-202 | schema builders and the document |
| `label`/`typeOf`/`matchesType`/`walk`/`validateConfig` | config-schema:209-361 | a recursive validator plus port-collision, password-length and duplicate-IP/dpr cross-checks |
| `at`/`resolveDevice`/`resolveKnownDevice`/`applyConfig` | apply-config:10-89 | path-labelled error wrapping and the serial push into `AppState` |
| `clampZoom`/`nearestIndex`/`zoomIn`/`zoomOut`/`formatZoom`/`applyZoom` | zoom:8-37 | the ladder; `nearestIndex` uses `forEach`, deliberately avoiding `.at()`/`findLastIndex` |
| `blobToDataUrl`/`loadAvatar`/`saveAvatar`/`clearAvatar` | avatar:5-42 | the `number[]` ↔ data-URL bridge |
| `cn`/`useFocus`/`buildQrValues`/`generatePassword`/`generateOtp`/`generateSlug`/`useInterval` | utils:33-123 | `generateOtp` correctly rejection-samples to avoid modulo bias; `generateSlug` and `useInterval` are dead |
| `readStoredLocale`/`getLocale`/`availableLocales`/`setLocale`/`lookup`/`interpolate`/`t`/`subscribe`/`useTranslation` | i18n:18-84 | see §9 |
| `walkthroughTargets`/`walkthroughSteps`/`HighlightProxy`/`scheduleReenter`/`WalkthroughArrow`/`WalkthroughCard` | walkthrough:67-287 | the tour |
| `clampOffset`/`draw`/`applyZoom`/`onPointerDown`/`onPointerMove`/`endDrag`/`handleSave` | avatar-crop-modal:34-163 | canvas crop with zoom-about-centre math |

---

## 3. Threads

The webview is single-threaded JavaScript. The only real threads are **web workers**, both monaco's,
both created lazily:

| worker | created at | lifetime | owns |
| --- | --- | --- | --- |
| monaco JSON worker | `monaco-setup.ts:32-39` factory, first requested by `preload-editor.ts:13` (~300 ms after Bootstrap mounts) or by `json-editor.tsx:175` on mount | kept alive by a 60 s ping per mounted editor (`json-editor.tsx:181`); monaco idles it out otherwise | JSON parsing, schema validation, hover and completion |
| monaco editor worker | same factory, `label !== "json"` | on demand | tokenization, diff, link detection |

In dev both are served as **classic IIFE workers** from the `/@classic-worker/` middleware
(`vite.config.ts:25-81`), each built on first request with `target: "safari13"` and memoized in a
`Map`; in production they are Vite `?worker` imports. Both entrypoints import
`@/lib/legacy-webview-polyfills` first (`monaco-editor.worker.ts:1`, `monaco-json.worker.ts:1`), so
the shims exist inside the workers too.

Pseudo-concurrency worth listing because it behaves like a thread:

| loop | where | cadence | lifetime |
| --- | --- | --- | --- |
| permission poll | `bootstrap.tsx:311` | 200 ms + on window `focus` | while the permission gate is open |
| JSON worker keepalive | `json-editor.tsx:181` | 60 s | per mounted editor (up to 3 during a conflict) |
| highlight tracker | `walkthrough.tsx:116-142` | every animation frame | only while a tour step is visible |
| walkthrough re-enter | `walkthrough.tsx:162` | 200 ms debounce | on resize / zoom |
| OTP spin | `settings.tsx:529` | one-shot 500 ms | on click |
| toast removal | `use-toast.ts:64` | one-shot 1,000,000 ms (~16 min) | per dismissed toast |

`useInterval` (`utils.ts:108`) exists but is never used.

---

## 4. Locks

No mutexes — this is single-threaded JS. The equivalents are the module-level mutable singletons and
the re-entrancy guards over them:

| guard | file:line | protects | contention risk |
| --- | --- | --- | --- |
| `snapshot` + `listeners` | config-provider:107-108 | the config store mirror | `publish` notifies **unconditionally**, including from pure reads |
| `loading` / `loadFailed` | config-provider:148-149 | dedupes the lazy first `getConfig` in `useConfig` | correct — the promise is nulled in `finally` |
| `ConfigDB` promise | config-provider:104 | the store handle | every caller awaits the same promise; a load failure is logged once, then rejects for every caller |
| `running` ref | bootstrap:77 | `start()` runs once | correct |
| `inFlight` / `stopped` | bootstrap:294-295 | prevents overlapping permission checks | correct |
| `walkthroughAutoStarted` | dashboard:31 | module-level, so the tour auto-starts once per **process**, not per mount | intentional; survives navigation |
| `started` promise | preload-editor:1 | the preload runs once | correct |
| `schemaRegistry` / `appliedSchemas` | json-editor:102-103 | monaco's global JSON schema list across editor instances | `applySchemas` diffs by identity before calling monaco |
| `cachedText` / `cachedStructure` | json-structure:36-37 | last-scan memo | a single-entry cache; the two conflict panes plus the main editor thrash it |
| `listeners` array | use-toast:128 | toast subscribers | mutated by `splice`/`push` on **every** state change (see §12 note) |
| `memoryState` | use-toast:130 | toast state outside React | — |
| `currentLocale` / `listeners` | i18n:28-29 | locale | — |
| `validateToken` | json-editor:332 | discards stale validation results | correct — compared before `setModelMarkers` |
| `seed` / `textRef` / `baseRef` | config-json-editor:106-109 | the editor's three-way merge state | `textRef`/`baseRef` are assigned **during render** (`:161-162`), which is technically a render side effect |

The nearest thing to a lock-ordering hazard is `refreshDecorations` (`json-editor.tsx:245`) being
called from eight different monaco event handlers plus a `requestAnimationFrame` (`:371`); it is
idempotent and guarded by `decorationKeyRef`, so re-entrancy is safe.

---

## 5. Buffers and copies

No frame or audio buffers pass through this territory. The notable copies:

| data | path | classification |
| --- | --- | --- |
| avatar PNG (~20-80 KB) | Rust `Vec<u8>` → JSON `number[]` over IPC → `new Uint8Array(bytes)` → `Blob` → `FileReader` → base64 data URL → React state → `<img src>` | **four full re-encodings**, one of them a ~3.5x JSON expansion. `avatar.ts:13-24`, `App.tsx:82` |
| avatar save | canvas → `toBlob` → `arrayBuffer` → `Uint8Array` → `Array.from(bytes)` → JSON → Rust | same expansion in reverse; `avatar-crop-modal.tsx:143-163`, `avatar.ts:26-33` |
| log lines | Rust `LogLine` event → JS string → `[...prev, line]` → `.slice(-2000)` → 2000 `<div>`s | **two array copies per line**, then a full React commit; `log-terminal.tsx:35-40` |
| config | store → 14 IPC values → merged object → `publish` → (editor) `JSON.stringify(_, null, 2)` | a fresh object per read, re-stringified per publish; `config-provider.tsx:122`, `config-json-editor.tsx:183` |
| QR codes | URL string → Reed-Solomon + matrix → SVG paths, per instance | 2N+2 instances per adapter set, none memoized; `dashboard.tsx:252` |
| known devices | `getConfig()` → `.map(d => ({...d, token: d.token ?? ""}))` → `[...knownDevices].sort(...)` **per render** | `config-provider.tsx:192`, `settings.tsx:345` |
| device overrides | `App.tsx:153-170` `prev.map` with a per-device `.find` over saved devices, on every config publish | O(devices x saved); correctly returns `prev` when nothing differs |
| editor byte count | `stats` walks the whole document char by char computing UTF-8 length | memoized on `[text]`, so once per keystroke; `config-json-editor.tsx:209-223` |
| JSON structure scan | full document re-scan on every `onDidChangeModelContent` | `json-editor.tsx:358-367`; memoized only against the immediately preceding text |

---

## 6. Queues

No channels or rings. The queue-shaped structures:

| structure | depth | bounded? | back-pressure |
| --- | --- | --- | --- |
| `LogTerminal` `lines` | 2000 (`MAX_LINES`) | yes, by truncation | **none** — every event causes an immediate O(n) copy and commit |
| `use-toast` `memoryState.toasts` | 1 (`TOAST_LIMIT`) | yes | older toasts are dropped by `.slice(0, 1)` |
| `toastTimeouts` map | unbounded | no | one ~16-minute timer per dismissed toast |
| `logbus` backlog (Rust side) | 2000 | yes | read once via `get_log_backlog` |
| Tauri event listeners | 8 in `App`, 1 in `Dashboard`, 1 in `LogTerminal`, 1 in `ThemeProvider`, 1 in `HighlightProxy` | — | no queue; each event dispatches synchronously into React state |
| permission poll | 1 in flight | yes (`inFlight`) | ticks are skipped, not queued — correct |
| JSON validation | 1 in flight (`validateToken`) | yes | stale results discarded — correct |

---

## 7. Syscall / FFI boundaries

Every crossing out of the webview:

**Tauri command IPC** (`bindings.ts` → `@tauri-apps/api/core` `invoke`). 43 commands; the ones on the
startup or control-plane hot paths:
- `setup`, `check_permissions`, `request_permission`, `open_permission_settings`,
  `check_system_requirements` — `bootstrap.tsx`
- `set_session_credentials`, `register_cloud_session`, `unregister_cloud_session`,
  `get_cloud_status`, `watch_for_network_changes`, `get_network_adapters`
- `set_device_override`, `set_device_audio_output`, `get_device_audio_outputs`,
  `set_device_banned`, `set_device_approved`, `refresh_device_stream`, `remove_device_override`
- `set_server_ports`, `set_turn_config`, `set_disconnect_grace`, `set_disable_gpu_encode`,
  `set_legacy_volume_key_proxy`, `get_display_capacity`, `set_window_zoom`
- `get_avatar`/`set_avatar`/`remove_avatar` — the `number[]` bridge
- `get_log_backlog`, `install_drivers`, `remove_drivers`, `install_audio_driver`,
  `uninstall_audio_driver`, `exit_app`

**Tauri event bridge** (`@tauri-apps/api/event` `listen`). 12 events declared; 10 subscribed:
`deviceJoin`, `deviceModify`, `deviceAudioOutputs`, `deviceRemove`, `networkChange`,
`sessionIdChange`, `hostedNetworkNoPassword`, `joinAttemptsPaused` (all `App.tsx`),
`cloudStatusChange` (`dashboard.tsx:86`), `logLine` (`log-terminal.tsx:35`).
`deviceModifyAction` and `deviceRemoveAction` are **declared in `bindings.ts` but never subscribed**
from `src/` — `deviceModify` and `deviceRemove` are also *emitted* from the frontend
(`device-details.tsx:217`, `:673`), which is an unusual bidirectional use of the event bus.

**Tauri plugin JS APIs**: `plugin-store` (config.json), `plugin-notification`
(`isPermissionGranted`/`requestPermission`/`sendNotification`), `plugin-os` (`type()` — called at
*module scope* in `settings.tsx:61-64`), `plugin-shell` (`Command.create("control"|"open")`,
`open(url)`), `plugin-clipboard-manager` (`writeText`), `plugin-updater` (`check`,
`downloadAndInstall`), `plugin-process` (`relaunch`), `plugin-autostart`
(`enable`/`disable`/`isEnabled`).

**Webview window API**: `getCurrentWebviewWindow()` at module scope in `App.tsx:30` and
`theme-provider.tsx:7`; `getCurrentWindow()` in `walkthrough.tsx:172`. Used for `isFocused`,
`onCloseRequested`, `onThemeChanged`, `onResized`.

**Browser APIs with version floors** (the 10.15 / Safari 13.1 constraint):
`crypto.getRandomValues` (11.1, safe), `navigator.clipboard.writeText`
(**13.1 exactly** — `config-json-editor.tsx:236`, at the floor), `MutationObserver` (safe),
`FileReader`/`Blob`/`canvas.toBlob` (safe), `URL.createObjectURL` (safe), `PointerEvent` +
`setPointerCapture` (13, safe), `localStorage` (guarded by try/catch in `i18n/index.ts` but **not**
in `App.tsx:215` or `settings.tsx:370`), `Web Workers` (safe), `matchMedia` (safe),
`requestAnimationFrame` (safe). Absent by design: `ResizeObserver` (13.1, available but unused),
`requestIdleCallback` (15.4, deliberately avoided), `structuredClone`, `Array.prototype.at` (shimmed
because monaco uses it), `Intl.*` beyond `toLocaleDateString`.

No dyld/link-time concern applies here — that rule governs the Rust/Objective-C side.

---

## 8. Panics

JavaScript has no panics; the equivalents are uncaught throws that unmount the tree (there is **no
error boundary anywhere in `src/`**, so any render-phase throw blanks the window) and non-null
assertions:

| site | condition | consequence |
| --- | --- | --- |
| `main.tsx:10` `document.getElementById("root")!` | `#root` missing | `createRoot(null)` throws; `index.html:34-52`'s fallback handler also bails because it needs `#root` → blank window |
| `bootstrap.tsx:155` `document.getElementById("dashlink")!.click()` | element absent when `runSetup` finishes | `TypeError` → outer `catch` (`:165`) → the **driver-install error dialog** is shown for a routing bug, and its retry loops |
| `bootstrap.tsx:93-94` `if (!existing) throw new Error("config store unreadable after setup")` | store read fails after a successful `setup()` | deliberate; caught at `:165` |
| `settings.tsx:229` `throw new Error("config store is empty")` | ditto | caught at `:249`, toasts |
| `config-json-editor.tsx:141` `throw new Error("config store is empty")` | ditto | caught, sets `loadError` |
| `ui/carousel.tsx:37` `throw new Error("useCarousel must be used within a <Carousel />")` | misuse | render-phase throw, unrecoverable |
| `theme-provider.tsx:111-112` `if (context === undefined) throw` | unreachable — `createContext` was given a non-undefined default (`:30`), so this can never fire |
| `device-details.tsx:246` `JSON.stringify(deviceDetails.values)` | a cyclic value | not reachable with the current `Device` shape |
| `walkthrough.tsx:106` `walkthroughTargets[currentStep]` | `currentStep` out of range | guarded by `if (!cfg)` at `:107` |
| `config-schema.ts:275` `new RegExp(pattern)` | a malformed `pattern` in the schema | the patterns are literals in the same file |
| `config-schema.ts:321-325` the `as` cast after `walk` | `walk` returned no errors but the shape is still wrong | `walk` validates `required` first, so the cast holds |

The highest-value fix is an error boundary at the root plus replacing the two `!` assertions.
See `minor-ui-runtime-panics-nonnull-assertions.md`.

---

## 9. Silent fallbacks and swallowed errors

Filed in full as `minor-ui-swallowed-errors-and-silent-fallbacks.md`. The ones that change behaviour
rather than just losing a message:

- **`ensurePermissions` conflates failure with success** (`bootstrap.tsx:206-211`): `catch { return false }`,
  and `false` means "nothing was missing", so a failed permission check lets startup proceed as if
  everything were granted.
- **`buildQrValues` returns `[]` on IPC failure** (`utils.ts:53-55`), which the dashboard renders as
  "Join or Create a Network (none were found)" (`dashboard.tsx:182`) — a wrong diagnosis.
- **`runUpdate` cannot distinguish "no update" from "check failed"** (`bootstrap.tsx:175-177`).
- **The JSON editor claims "Valid configuration"** (`config-json-editor.tsx:443-448`) when the
  worker never started, because `doValidation`'s failure is `.catch(() => {})`
  (`json-editor.tsx:345`) and no markers ever arrive.
- **`requestPermission` failures are ignored in a loop** (`bootstrap.tsx:215`).
- **`isWifiOn` failure defaults to `true`** (`settings.tsx:585-587`), suppressing the "turn on Wi-Fi"
  dialog.
- **`minify()` silently does nothing on invalid JSON** (`json-editor.tsx:311-312`).
- **The entire monaco preload is `catch {}`** (`preload-editor.ts:36`) — correct in intent, but a
  broken chunk is only discovered when the user opens Advanced.
- ~45 `console.error` calls that go nowhere in a release build (no devtools), while the app has a
  perfectly good log bus the frontend never writes to.

Correct, deliberate swallows worth noting so they are not "fixed": the two `localStorage` guards in
`i18n/index.ts:22-24`/`:44-46` (both with explanatory comments), and `settings.tsx:852`'s
`getConfig().catch(() => undefined)`, which fails toward *showing* the confirmation dialog.

---

## 10. TODO / FIXME / HACK / XXX

**None.** `grep -rn "TODO\|FIXME\|HACK\|XXX" src/` over `.ts`, `.tsx`, `.json` and `.css` returns
zero hits. The only commented-out code is the 17-line `CheckSelect` at
`device-details.tsx:768-784`. Two `@ts-expect-error` comments carry real information:
`device-details.tsx:579` and `:647` ("no children for tooltip"), and `ui/carousel.tsx:3-8` has four
`// @ts-expect-error type-err` suppressions on embla's type imports.

---

## 11. Latency path

**Nothing in `src/` is on the glass-to-glass, input or audio path.** Once a device has joined, the
video pipeline runs entirely in Rust and the client page; this UI only issues control commands.

It *is* the entire **application startup path**, and it contributes to two others:

### Startup, serialized (`main.tsx` → usable dashboard)

| stage | file:line | cost | serialization point |
| --- | --- | --- | --- |
| polyfills evaluate | `main.tsx:1` → `legacy-webview-polyfills.ts:206-212` | ~1 ms; 2 probe `new RegExp` in try/catch | blocks the entry chunk |
| entry chunk parse + evaluate | one ~914 KB chunk (measured in `major-build-single-eager-startup-chunk.md`) | dominant on the 2015 Intel floor | blocks first paint |
| `createRoot().render()` | `main.tsx:10` | — | first paint of "Starting…" |
| **`await check()`** (updater HTTP) | `bootstrap.tsx:275` | a network round trip, or the full timeout offline | **blocks everything below** |
| `checkPermissions` | `bootstrap.tsx:208` | macOS TCC/AX lookups | blocks |
| `checkSystemRequirements` | `bootstrap.tsx:249` | Rust-side probing | blocks |
| **`commands.setup()`** | `bootstrap.tsx:83` | the real work: virtual display + `AppState` | blocks |
| `getConfig()` x3 | `bootstrap.tsx:90,93,151` | **45 serialized IPC round-trips** | blocks |
| device replay loop | `bootstrap.tsx:105-127` | 1-2 IPCs **per saved device**, serial, unbounded | blocks |
| `watch_for_network_changes`, `set_session_credentials` | `:131,136` | 2 IPCs | blocks |
| `buildQrValues` → `get_network_adapters` | `:149` | 1 IPC | blocks |
| `setTurnConfig` | `:153` | 1 IPC | blocks |
| navigate | `:155` | — | dashboard mounts |
| Dashboard `getConfig()` + `get_cloud_status` + `get_display_capacity` | `dashboard.tsx:68,94,121` | 17 IPCs; render gated on `statusLoaded` | **the dashboard renders empty until `get_cloud_status` resolves** |
| **monaco preload** (concurrent) | `preload-editor.ts:40`, T+300 ms | ~2.2 s warm / 4.3 s cold main-thread work, per the recovered measurements | competes with everything above |

Estimated total config IPC alone: **~75 serialized round-trips**. Every one of the "blocks" rows is a
strict serialization point where nothing else runs.

### Join path contribution
When a device joins, `deviceJoin` (`App.tsx:225`) fires `recordKnownDevice` (a full `getConfig()` +
`updateConfig`, ~16 IPCs) and `set_device_approved`, then re-renders the tree. `deviceModify`
(`:257`) reallocates `devices`, which triggers `get_display_capacity` (`dashboard.tsx:121`) and, on
Settings, another full `getConfig()` (`settings.tsx:270`). These run **concurrently with the host
negotiating WebRTC and starting capture** — main-thread scripting in the same process.

### Control-plane latency
`set_device_override` (from the device editor's Save) is a single IPC; the perceived latency is the
Rust-side epoch bump, not this code. `refresh_device_stream` likewise.

---

## 12. Stale references

- **`CLAUDE.md` omits two shipped commands.** `set_window_zoom` (`bindings.ts:39`, used by
  `zoom.ts:33`) and `get_display_capacity` (`bindings.ts:125`, used by `dashboard.tsx:121` and
  gating the whole dashboard render) are absent from the "Registered commands" list. Filed as
  `minor-docs-claudemd-drifted-from-bindings.md`.
- **`CLAUDE.md` says "Current version: 0.5.4"**; `package.json:4` says `0.6.3`.
- **`CLAUDE.md`'s client-page file list omits `styles.css` context** — not a `src/` issue, noted for
  the orchestrator.
- **`GlobalContextDefault`** (`global-provider.tsx:10-21`) is exported and never imported; it
  duplicates the defaults already inlined into `createContext` at `:37-48`, and the two have drifted
  apart in shape (one uses bare keys, the other `window*` tuples).
- **`deviceModifyAction` / `deviceRemoveAction`** exist in `bindings.ts:159,161` and are never
  subscribed or emitted from `src/`.
- **`config.sessionPassword`** is in the type, the default and the schema, described there as
  "Reserved for a fixed session password… so this is normally empty"
  (`config-schema.ts:139`) — it is read by nothing in `src/`.
- **`window.sidebarSize` / `window.otp`** are declared as globals (`utils.ts:27-28`) and never
  assigned; the sidebar has kept its width in React state since `layout.tsx:13`.
- **`Config.dontShowAgain.configEditor` is optional** (`config-provider.tsx:65`, `?`) while the other
  three are required, and `config-schema.ts:192` omits it from `required` — a migration artifact.
- **No AI-tooling references** found anywhere in `src/`.
- `en.json` has a `$comment` top-level key (`i18n/locales/en.json`) — worth checking it still
  describes the file accurately.

---

## 13. Dead code candidates

| candidate | evidence |
| --- | --- |
| `src/components/ui/resizable.tsx` | zero importers; sole importer of `react-resizable-panels` and `@radix-ui/react-icons` |
| `src/components/ui/form.tsx` | zero importers; sole importer of `react-hook-form` |
| `utils.ts:108-123` `useInterval` | zero call sites; both real polls hand-roll `setInterval` |
| `utils.ts:96-106` `generateSlug` | zero call sites; also the only randomness helper using `Math.random()` |
| `utils.ts:27-28` `Window.sidebarSize` / `Window.otp` | never assigned or read |
| `global-provider.tsx:10-21` `GlobalContextDefault` | exported, never imported |
| `device-details.tsx:768-784` `CheckSelect` | fully commented out |
| `theme-provider.tsx:111-112` | unreachable — the context has a non-undefined default |
| deps `zod`, `@hookform/resolvers`, `react-avatar`, `react-svg`, `@radix-ui/react-tooltip`, `@tauri-apps/plugin-dialog` | no JS imports (`motion` looks unused but is a real `nextstepjs` peer — do not remove) |
| `table.tsx:76,90` `[&:has([role=checkbox])]` | no `role="checkbox"` is ever placed in a table cell, and `:has()` is dropped on the floor anyway |
| `Config.sessionPassword` | never read |

Filed as `minor-ui-dead-components-and-unused-dependencies.md` (components + deps, by a prior agent)
and `minor-ui-dead-helpers-and-globals-in-utils.md` (helpers, globals, commented-out code).

---

## 14. Capability probing

The frontend's own probing is **mostly correct**, with one structural defect:

**Correct — probe by capability:**
- `legacy-webview-polyfills.ts` `define()` (`:75-79`) installs a shim only when the member is not
  already a function. `shimRegExpFlags` (`:133-141`) genuinely *probes* by constructing
  `new RegExp("", "d")` in a try/catch and returns early when the flags are accepted, so modern
  hosts keep the native `RegExp`. This is exactly the tier doctrine applied in JS.
- `bootstrap.tsx` reads `check_system_requirements`'s `CompatibilityReport` and branches on
  `os_supported` and per-API `severity` (`:254-266`) rather than on a version number — the version
  strings it shows (`os_version`, `min_os_version`) are display-only.
- `device-details.tsx:120-122` branches on the **backend tier string**
  (`needs_driver_install` / `unsupported` / `virtual_device`) reported by the Rust probe, not on a
  macOS version.
- `dashboard.tsx:140` uses `get_display_capacity`'s `full` flag rather than assuming the AirPlay
  tier.

**Defect — probe by OS name:**
- `settings.tsx:61-64` decides three things from `getOsType()` at **module scope**:
  ```ts
  const MIN_HOSTED_NETWORK_PASSWORD_LENGTH = getOsType() === "macos" ? 10 : 8;
  const IS_WINDOWS = getOsType() === "windows";
  const IS_MACOS  = getOsType() === "macos";
  const SUPPORTS_WIFI_QR = IS_WINDOWS || IS_MACOS;
  ```
  `IS_WINDOWS` then gates the entire **Video Encoding / disable-GPU-encode card** (`:1294`), so a
  macOS or Linux user has no way to force software encode even though `set_disable_gpu_encode` is
  registered on all three platforms. That is an OS check standing in for a capability check: the
  right signal is whether the active encoder backend has a software fallback, which
  `CompatibilityReport` could carry. `SUPPORTS_WIFI_QR` has the same shape (Linux is excluded by
  name, not by asking whether a hosted network can be created).

**Missing tier:** there is no UI surface for the `CompatibilityReport.display_backend` field at all —
`bootstrap.tsx` reads `os_supported` and `unsupported_apis` but never `display_backend`, and only
`device-details.tsx` reads `audio_backend`. A user on the macOS AirPlay display tier learns about it
only indirectly, through `capacity.full` rendering a hardcoded English paragraph
(`dashboard.tsx:144-157`).

---

## 15. Questions and cross-cutting concerns

1. **Is the ~75-IPC startup config storm visible in the Rust command log?** The frontend counts are
   derived from reading `plugin-store`'s implementation, not measured. Someone with a Windows or
   macOS build should confirm the actual invoke count and per-invoke cost before this is prioritized
   against the encode-path work. (M1 has no measurement rig; the app has no frontend timing
   instrumentation at all.)

2. **`get_avatar`/`set_avatar` cross as `number[]`.** Changing them to base64 `String` needs a Rust
   signature change + a `bindings.ts` regeneration. That regeneration only happens on a debug build
   with **no CLI arguments**, and it silently reverts the API surface if run against a stale binary
   (per the user's `bindings-regeneration-target-dir` memory). Whoever owns the Rust side should
   batch this with any other command changes rather than regenerating twice.

3. **A batched device-replay command would help both frontends.** `bootstrap.tsx:105-127` and
   `cli.rs`'s `apply_saved_devices` do the same serial replay. A single
   `set_device_overrides(Vec<_>)` / `set_device_trust(Vec<_>)` pair would serve both. That is a
   cross-territory change: `collect_commands!` plus all three `*_utils` modules.

4. **The frontend has no path into the log bus.** `CLAUDE.md` mandates `tprintln!` over `println!`
   in the Rust core precisely so logs reach the UI, but ~45 frontend `console.error` calls go
   nowhere in a release build. A `log_frontend_error(String)` command feeding `tprintln!` would
   close the loop and make `ScreenExtend logs` complete. Cheap, and it makes every other finding in
   this territory diagnosable in the field.

5. **`deviceModify` / `deviceRemove` are emitted *from* the frontend**
   (`device-details.tsx:217`, `:673`) as well as consumed. Whoever owns the event bus should confirm
   this is intended — it means the host's own view of a device can be changed by a webview message,
   and `deviceModifyAction`/`deviceRemoveAction` (which look like they were meant to be the
   frontend→backend direction) are unused.

6. **The monaco preload is a measured, deliberate trade that predates the latency mandate.** The
   rationale survives only in a dangling commit (`major-ui-monaco-preload-measurements-stripped-from-the-tree.md`).
   Before anyone "fixes" it, the orchestrator should decide whether ~2-4 s of startup main-thread
   work in exchange for an instant Settings editor is the right call for a latency-critical host app.
   I lean no, but the decision is above this territory.

7. **Nothing in `src/` is on the glass-to-glass path**, so the ROI here is entirely (a) time to a
   usable dashboard, (b) main-thread contention with capture/encode while the app is streaming, and
   (c) the Safari 13.1 compatibility floor. If the engagement has to choose, (b) is the one that
   touches the latency mandate: this UI re-renders its whole tree on every toast, every device
   event and every config read, in the same process that is encoding video.

8. **`useToast` is the single highest-leverage frontend fix.** It is a one-line dependency-array
   change plus a mechanical import swap at 12 call sites, and it removes the largest re-render
   amplifier in the app. It should go in before any of the context-splitting work, because it makes
   the context problem measurable in isolation.
