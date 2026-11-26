import { createContext, useContext } from "react";

// The eager half of the walkthrough: nothing here may import `nextstepjs` or `motion`.
// Boot-path code starts the tour through `useWalkthrough()`, so the library stays out of
// the entry chunk until a tour is asked for.

export const WALKTHROUGH_TOUR = "welcome";
export const HIGHLIGHT_PROXY_ID = "tour-highlight-proxy";

export type WalkthroughControl = {
  /** Load the tour chunk if it is not loaded, then run `tour` from its first step. */
  start: (tour: string) => void;
};

const noop: WalkthroughControl = { start: () => {} };

export const WalkthroughContext = createContext<WalkthroughControl>(noop);

export function useWalkthrough(): WalkthroughControl {
  return useContext(WalkthroughContext);
}
