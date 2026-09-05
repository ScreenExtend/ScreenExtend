type LegacyMediaQueryList = MediaQueryList & {
  addListener?(listener: (event: MediaQueryListEvent) => void): void;
  removeListener?(listener: (event: MediaQueryListEvent) => void): void;
};

type ListenerBridge = (
  this: MediaQueryList,
  type: string,
  listener: EventListenerOrEventListenerObject | null,
) => void;

function bridgeMediaQueryListEvents() {
  if (typeof MediaQueryList === "undefined") return;

  const proto = MediaQueryList.prototype as LegacyMediaQueryList;
  if (typeof proto.addEventListener === "function") return;
  if (typeof proto.addListener !== "function") return;

  const bridges = new WeakMap<
    MediaQueryList,
    Map<EventListenerOrEventListenerObject, (event: MediaQueryListEvent) => void>
  >();

  const target = proto as unknown as {
    addEventListener: ListenerBridge;
    removeEventListener: ListenerBridge;
  };

  target.addEventListener = function (type, listener) {
    if (type !== "change" || !listener) return;
    const list = this as LegacyMediaQueryList;
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
    list.addListener?.(bridge);
  };

  target.removeEventListener = function (type, listener) {
    if (type !== "change" || !listener) return;
    const list = this as LegacyMediaQueryList;
    const registered = bridges.get(list);
    const bridge = registered?.get(listener);
    if (!registered || !bridge) return;
    registered.delete(listener);
    list.removeListener?.(bridge);
  };
}

bridgeMediaQueryListEvents();
