import type { ArrowComponentProps, CardComponentProps, Step, Tour } from "nextstepjs";
import { useCallback, useContext, useEffect, useLayoutEffect, useRef } from "react";
import { useNextStep } from "nextstepjs";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { X } from "lucide-react";

import { Card } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { GlobalProviderContext } from "@/components/global-provider";
import { cn } from "@/lib/utils";

export const WALKTHROUGH_TOUR = "welcome";
export const HIGHLIGHT_PROXY_ID = "tour-highlight-proxy";
export type SidePadding = { top?: number; right?: number; bottom?: number; left?: number };

interface WalkStep {
  title: string;
  content: string;
  target: string;
  padding?: SidePadding;
  side?: Step["side"];
  pointerRadius?: number;
}

const DEFAULT_PADDING: Required<SidePadding> = { top: 10, right: 10, bottom: 10, left: 10 };

const rawSteps: WalkStep[] = [
  {
    title: "Welcome to ScreenExtend",
    content: "ScreenExtend turns any phone, tablet, or laptop into a wireless second monitor for this PC. This covers the basics of the user interface.",
    target: "#tour-brand",
    padding: { top: -5, right: 15, bottom: -5, left: 15 },
    side: "bottom-left",
  },
  {
    title: "Connect a device",
    content: "On the device you want to use as a screen, scan one of these QR codes or open the link. Pick the network the device is on: local Wi-Fi for speed, or \"Anywhere (Internet)\" to join from another network.",
    target: "#tour-connect",
    padding: { top: 10, right: 0, bottom: 10, left: 0 },
  },
  {
    title: "Navigation bar",
    content: "Add Device has these join codes, Edit Device allows adjustments of connected screens' scale, orientation and refresh rate, and Settings has networks, security and more.",
    target: "#tour-nav",
    side: "right",
  },
  {
    title: "Session ID & one-time code",
    content: "Joining devices enter this Session ID and OTP to pair securely. You can refresh the OTP any time from the Settings page.",
    target: "#tour-session",
    side: "top-left",
  },
  {
    title: "Light or dark",
    content: "Switch between light, dark, or matching your system theme here.",
    target: "#tour-theme",
    side: "top-right",
  },
  {
    title: "Your account",
    content: "Update your name and photo, reset preferences, or exit the app from the profile menu. That's it for the tour; enjoy your extra screen!",
    target: "#tour-profile",
    side: "bottom-right",
  },
];

export const walkthroughTargets = rawSteps.map((s) => ({
  target: s.target,
  padding: { ...DEFAULT_PADDING, ...s.padding } as Required<SidePadding>,
}));

export const walkthroughSteps: Tour[] = [
  {
    tour: WALKTHROUGH_TOUR,
    steps: rawSteps.map((s) => ({
      title: s.title,
      content: s.content,
      selector: `#${HIGHLIGHT_PROXY_ID}`,
      side: s.side,
      pointerPadding: 0,
      pointerRadius: s.pointerRadius ?? 12,
    })),
  },
];

export function HighlightProxy() {
  const {
    currentStep,
    currentTour,
    isNextStepVisible,
    closeNextStep,
    startNextStep,
    setCurrentStep,
  } = useNextStep();
  const { windowZoom: [zoom] } = useContext(GlobalProviderContext);
  const proxyRef = useRef<HTMLDivElement>(null);

  useLayoutEffect(() => {
    const proxy = proxyRef.current;
    if (!proxy) return;
    const active = isNextStepVisible && currentTour === WALKTHROUGH_TOUR;
    const cfg = active ? walkthroughTargets[currentStep] : undefined;
    if (!cfg) {
      proxy.style.display = "none";
      return;
    }

    const MARGIN = 3;
    let raf = 0;
    let last = "";

    const tick = () => {
      const target = document.querySelector(cfg.target);
      if (target) {
        const r = target.getBoundingClientRect();
        const { top, right, bottom, left } = cfg.padding;
        const l = Math.max(r.left - left, MARGIN);
        const t = Math.max(r.top - top, MARGIN);
        const rt = Math.min(r.right + right, window.innerWidth - MARGIN);
        const bt = Math.min(r.bottom + bottom, window.innerHeight - MARGIN);
        const w = Math.max(rt - l, 0);
        const h = Math.max(bt - t, 0);
        const key = `${l},${t},${w},${h}`;
        if (key !== last) {
          last = key;
          proxy.style.display = "block";
          proxy.style.left = `${l}px`;
          proxy.style.top = `${t}px`;
          proxy.style.width = `${w}px`;
          proxy.style.height = `${h}px`;
        }
      } else if (last !== "hidden") {
        last = "hidden";
        proxy.style.display = "none";
      }
      raf = requestAnimationFrame(tick);
    };
    tick();
    return () => cancelAnimationFrame(raf);
  }, [currentStep, currentTour, isNextStepVisible]);

  const stateRef = useRef({ currentStep, currentTour, isNextStepVisible });
  stateRef.current = { currentStep, currentTour, isNextStepVisible };

  const settleRef = useRef(0);
  const pendingRef = useRef(false);
  const savedStepRef = useRef(0);

  const scheduleReenter = useCallback(() => {
    if (!pendingRef.current) {
      const s = stateRef.current;
      if (!s.isNextStepVisible || s.currentTour !== WALKTHROUGH_TOUR) return;
      pendingRef.current = true;
      savedStepRef.current = s.currentStep;
      closeNextStep();
    }
    clearTimeout(settleRef.current);
    settleRef.current = window.setTimeout(() => {
      pendingRef.current = false;
      startNextStep(WALKTHROUGH_TOUR);
      setCurrentStep(savedStepRef.current);
    }, 200);
  }, [closeNextStep, startNextStep, setCurrentStep]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    void getCurrentWindow()
      .onResized(() => scheduleReenter())
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [scheduleReenter]);

  const zoomReady = useRef(false);
  useEffect(() => {
    if (!zoomReady.current) {
      zoomReady.current = true;
      return;
    }
    scheduleReenter();
  }, [zoom, scheduleReenter]);

  useEffect(() => () => clearTimeout(settleRef.current), []);

  return (
    <div
      id={HIGHLIGHT_PROXY_ID}
      ref={proxyRef}
      aria-hidden="true"
      style={{
        position: "fixed",
        left: 0,
        top: 0,
        width: 0,
        height: 0,
        pointerEvents: "none",
        display: "none",
      }}
    />
  );
}

export function WalkthroughArrow({ style }: ArrowComponentProps) {
  return (
    <svg
      viewBox="0 0 54 54"
      style={{ ...style, width: "1.5rem", height: "1.5rem", overflow: "visible" }}
    >
      <path
        d="M0 0 L27 27 L0 54"
        fill="hsl(var(--card))"
        stroke="hsl(var(--border))"
        strokeWidth={3}
        strokeLinejoin="round"
      />
    </svg>
  );
}

export function WalkthroughCard({
  step,
  currentStep,
  totalSteps,
  nextStep,
  prevStep,
  skipTour,
  arrow,
}: CardComponentProps) {
  const isFirst = currentStep === 0;
  const isLast = currentStep === totalSteps - 1;

  return (
    <Card className="w-[340px] max-w-[90vw] p-5 shadow-xl">
      <div className="flex items-start gap-3">
        <h3 className="flex-1 text-base font-semibold leading-tight pt-0.5">{step.title}</h3>
        {skipTour && !isLast && (
          <button
            type="button"
            onClick={skipTour}
            aria-label="Skip tour"
            className="-mr-1 -mt-1 rounded-md p-1 text-muted-foreground transition-colors hover:text-foreground"
          >
            <X size={16} />
          </button>
        )}
      </div>
      <p className="mt-2 text-sm leading-relaxed text-muted-foreground">{step.content}</p>
      <div className="mt-4 flex items-center justify-between">
        <div className="flex items-center gap-1.5">
          {Array.from({ length: totalSteps }).map((_, i) => (
            <span
              key={i}
              className={cn(
                "h-1.5 rounded-full transition-all",
                i === currentStep ? "w-4 bg-primary" : "w-1.5 bg-muted"
              )}
            />
          ))}
        </div>
        <div className="flex items-center gap-2">
          {!isFirst && (
            <Button variant="ghost" size="sm" onClick={prevStep}>
              Back
            </Button>
          )}
          <Button size="sm" onClick={nextStep}>
            {isLast ? "Done" : "Next"}
          </Button>
        </div>
      </div>
      {arrow}
    </Card>
  );
}
