import { useEffect } from "react";
import { NextStepProvider, NextStepReact, useNextStep } from "nextstepjs";

import {
  HighlightProxy,
  WalkthroughArrow,
  WalkthroughCard,
  walkthroughSteps,
} from "@/components/walkthrough";

// Everything `nextstepjs` needs, behind one dynamic import. `NextStepReact` portals its
// overlay to `document.body`, so it can be a sibling of the app. `App.tsx` renders the
// `nextstep-wrapper` > `nextstep-site` pair statically, so the containing block for
// `#innerSidebar` exists whether or not the tour ever opens.

function AutoStart({ tour }: { tour: string }) {
  const { startNextStep } = useNextStep();
  // runs after HighlightProxy commits, so the `#tour-highlight-proxy` every step points at
  // is in the document before the first step is measured
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
