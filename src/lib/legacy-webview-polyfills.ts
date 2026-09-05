// macOS 10.15's WKWebView is Safari 13.1, and the app's minimumSystemVersion is
// 10.15, so anything newer that a dependency reaches for is a hard crash rather
// than a missing feature. Monaco alone needs five of these; the MediaQueryList
// one runs at module scope, which takes down the whole settings editor chunk.
//
// `node scripts/check-legacy-webkit.mjs` re-derives this list from the build
// output. Keep the two in step, and keep every shim safe to run in a worker as
// well as on the page: the monaco workers import this too.

type LegacyMediaQueryList = MediaQueryList & {
  addListener?(listener: (event: MediaQueryListEvent) => void): void;
  removeListener?(listener: (event: MediaQueryListEvent) => void): void;
};

type ListenerBridge = (
  this: MediaQueryList,
  type: string,
  listener: EventListenerOrEventListenerObject | null,
) => void;

const bridges = new WeakMap<
  MediaQueryList,
  Map<EventListenerOrEventListenerObject, (event: MediaQueryListEvent) => void>
>();

const addBridged: ListenerBridge = function (type, listener) {
  if (type !== "change" || !listener) return;
  const list = this as LegacyMediaQueryList;
  if (typeof list.addListener !== "function") return;
  let registered = bridges.get(list);
  if (!registered) {
    registered = new Map();
    bridges.set(list, registered);
  }
  if (registered.has(listener)) return;
  const bridge = (event: MediaQueryListEvent) => {
    if (typeof listener === "function") listener.call(list, event);
    else listener.handleEvent(event);
  };
  registered.set(listener, bridge);
  list.addListener(bridge);
};

const removeBridged: ListenerBridge = function (type, listener) {
  if (type !== "change" || !listener) return;
  const list = this as LegacyMediaQueryList;
  const registered = bridges.get(list);
  const bridge = registered?.get(listener);
  if (!registered || !bridge) return;
  registered.delete(listener);
  list.removeListener?.(bridge);
};

// Safari 13's MediaQueryList does not inherit from EventTarget. Patch the
// prototype when it is reachable and every instance matchMedia hands out in
// case it is not, and install the methods even where there is no legacy
// addListener to bridge to, so the call is inert instead of fatal.
function bridgeMediaQueryListEvents() {
  const shim = (list: MediaQueryList | null) => {
    if (!list || typeof list.addEventListener === "function") return list;
    const target = list as unknown as {
      addEventListener: ListenerBridge;
      removeEventListener: ListenerBridge;
    };
    target.addEventListener = addBridged;
    target.removeEventListener = removeBridged;
    return list;
  };

  if (typeof MediaQueryList !== "undefined") shim(MediaQueryList.prototype);

  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
  const nativeMatchMedia = window.matchMedia;
  const patched = function (query: string) {
    return shim(nativeMatchMedia.call(window, query)) as MediaQueryList;
  };
  try {
    Object.defineProperty(window, "matchMedia", {
      value: patched,
      writable: true,
      configurable: true,
    });
  } catch {
    window.matchMedia = patched;
  }
}

function define(target: object, name: string, value: unknown) {
  const holder = target as Record<string, unknown>;
  if (typeof holder[name] === "function") return;
  Object.defineProperty(target, name, { value, writable: true, configurable: true });
}

// Safari 14.1. Monaco only uses it to avoid pinning windows and editor views
// alive, so a strong reference is correct, just less tidy.
function shimWeakRef() {
  class StrongRef<T extends object> {
    private readonly target: T;
    constructor(target: T) {
      this.target = target;
    }
    deref(): T | undefined {
      return this.target;
    }
  }
  define(globalThis, "WeakRef", StrongRef);
}

// Safari 14. Thrown by monaco's DisposableStore when several disposables fail.
function shimAggregateError() {
  class AggregateErrorShim extends Error {
    readonly errors: unknown[];
    constructor(errors: Iterable<unknown>, message?: string) {
      super(message);
      this.name = "AggregateError";
      this.errors = Array.from(errors);
    }
  }
  define(globalThis, "AggregateError", AggregateErrorShim);
}

// Safari 14.
function shimReplaceChildren() {
  const replaceChildren = function (this: ParentNode, ...nodes: (Node | string)[]) {
    while (this.firstChild) this.removeChild(this.firstChild);
    if (nodes.length) this.append(...nodes);
  };
  for (const owner of [
    typeof Element === "undefined" ? null : Element,
    typeof Document === "undefined" ? null : Document,
    typeof DocumentFragment === "undefined" ? null : DocumentFragment,
  ]) {
    if (owner) define(owner.prototype, "replaceChildren", replaceChildren);
  }
}

// Relative indexing, Safari 15.4.
function shimRelativeIndexing() {
  const at = function (this: { length: number; [index: number]: unknown }, index: number) {
    const length = this.length;
    const offset = Math.trunc(index) || 0;
    const resolved = offset < 0 ? length + offset : offset;
    return resolved < 0 || resolved >= length ? undefined : this[resolved];
  };
  define(Array.prototype, "at", at);
  define(String.prototype, "at", at);
}

// Safari 15.4.
function shimFindLast() {
  type Probe = (value: unknown, index: number, array: unknown[]) => unknown;
  const findLastIndex = function (this: unknown[], probe: Probe, thisArg?: unknown) {
    for (let i = this.length - 1; i >= 0; i--) {
      if (probe.call(thisArg, this[i], i, this)) return i;
    }
    return -1;
  };
  const findLast = function (this: unknown[], probe: Probe, thisArg?: unknown) {
    const index = findLastIndex.call(this, probe, thisArg);
    return index === -1 ? undefined : this[index];
  };
  define(Array.prototype, "findLast", findLast);
  define(Array.prototype, "findLastIndex", findLastIndex);
}

bridgeMediaQueryListEvents();
shimWeakRef();
shimAggregateError();
shimReplaceChildren();
shimRelativeIndexing();
shimFindLast();
