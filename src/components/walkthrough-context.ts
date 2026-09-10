import { createContext, useContext } from "react";

// The eager half of the walkthrough. Nothing here imports `nextstepjs` or `motion`, which
// is the whole point: `@/components/walkthrough` and `@/components/walkthrough-mount` do,
// and they are only reached through the dynamic import in `App.tsx`. Anything on the boot
// path that wants to start the tour -- the dashboard's first-run check, the profile
// menu's "Replay Tour" -- goes through `useWalkthrough()` instead of `useNextStep()`, so
// the library stays out of the entry chunk until someone actually asks for a tour.

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
