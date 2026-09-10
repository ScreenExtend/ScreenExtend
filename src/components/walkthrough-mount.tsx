import { useEffect } from "react";
import { NextStepProvider, NextStepReact, useNextStep } from "nextstepjs";

import {
  HighlightProxy,
  WalkthroughArrow,
  WalkthroughCard,
  walkthroughSteps,
} from "@/components/walkthrough";

// Everything `nextstepjs` needs, behind one dynamic import. `NextStepReact` renders its
// overlay, pointer and card through `DynamicPortal`, which portals to `document.body` --
// so it does not have to be an ancestor of the app to work, and here it is a sibling. Its
// only structural contribution was `div[data-name=nextstep-wrapper]` >
// `div[data-name=nextstep-site]`, and `App.tsx` renders that pair statically so the
// containing block for `#innerSidebar` and every other absolutely-positioned descendant
// is there whether the tour has ever been opened or not.

function AutoStart({ tour }: { tour: string }) {
  const { startNextStep } = useNextStep();
  // Runs after `HighlightProxy` has committed, so `#tour-highlight-proxy` -- the selector
  // every step points at -- is in the document before the first step is measured.
  useEffect(() => {
    startNextStep(tour);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tour]);
  return null;
}

export default function WalkthroughMount({
  tour,
  onFinish,
}: {
  tour: string;
  onFinish: () => void;
}) {
  return (
    <NextStepProvider>
      <NextStepReact
        steps={walkthroughSteps}
        cardComponent={WalkthroughCard}
        onComplete={onFinish}
        onSkip={onFinish}
        shadowOpacity="0.5"
        arrowComponent={WalkthroughArrow}
        cardTransition={{ ease: "easeInOut", duration: 0.4 }}
        disableConsoleLogs
      >
        <HighlightProxy />
        <AutoStart tour={tour} />
      </NextStepReact>
    </NextStepProvider>
  );
}
