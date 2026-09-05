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

function shimWeakRef() {
  class StrongRef<T extends object> {
    private declare target: T;
    constructor(target: T) {
      this.target = target;
    }
    deref(): T | undefined {
      return this.target;
    }
  }
  define(globalThis, "WeakRef", StrongRef);
}

function shimAggregateError() {
  class AggregateErrorShim extends Error {
    declare errors: unknown[];
    constructor(errors: Iterable<unknown>, message?: string) {
      super(message);
      this.name = "AggregateError";
      this.errors = Array.from(errors);
    }
  }
  define(globalThis, "AggregateError", AggregateErrorShim);
}

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

function shimRegExpFlags() {
  const Native = RegExp;
  const rejected = ["d", "v"].filter(flag => {
    try {
      new Native("", flag);
      return false;
    } catch {
      return true;
    }
  });
  if (!rejected.length) return;

  const locate = (match: RegExpExecArray) => {
    const whole = match[0];
    const base = match.index;
    const spans: ([number, number] | undefined)[] = [[base, base + whole.length]];
    let cursor = 0;
    for (let group = 1; group < match.length; group++) {
      const text = match[group];
      const at = text === undefined ? -1 : whole.indexOf(text, cursor);
      if (at === -1) {
        spans.push(undefined);
        continue;
      }
      spans.push([base + at, base + at + text.length]);
      cursor = at;
    }
    return spans;
  };

  const nativeExec = Native.prototype.exec;
  const withIndices = (expression: RegExp) => {
    expression.exec = function (this: RegExp, input: string) {
      const match = nativeExec.call(this, input);
      if (match) (match as RegExpExecArray & { indices?: unknown }).indices = locate(match);
      return match;
    };
    return expression;
  };

  const Patched = function (pattern: string | RegExp, flags?: string) {
    if (typeof flags !== "string") return new Native(pattern as string, flags);
    let kept = flags;
    for (const flag of rejected) {
      if (kept.indexOf(flag) >= 0) kept = kept.split(flag).join("");
    }
    if (kept === flags) return new Native(pattern as string, flags);
    return withIndices(new Native(pattern as string, kept));
  } as unknown as RegExpConstructor;

  (Patched as unknown as { prototype: RegExp }).prototype = Native.prototype;
  Object.defineProperty(Native.prototype, "constructor", {
    value: Patched,
    writable: true,
    configurable: true,
  });
  (globalThis as { RegExp: RegExpConstructor }).RegExp = Patched;
}

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
shimRegExpFlags();
