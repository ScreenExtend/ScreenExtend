import { useEffect, useState, useContext, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Link } from "react-router-dom";

import Layout from "@/layout/layout";
import { Card } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Copy, Check } from "lucide-react";
import QRCode from "react-qr-code";
import {
  Carousel,
  CarouselContent,
  CarouselItem,
  CarouselNext,
  CarouselPrevious,
} from "@/components/ui/carousel";

import { GlobalProviderContext } from "@/components/global-provider";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { commands, events } from "@/lib/bindings";
import { buildCloudQrValue } from "@/lib/utils";

type CloudStatus = { state: string; detail: string };

function CloudBadge({ status }: { status: CloudStatus }) {
  const map: Record<string, { label: string; className: string }> = {
    registered: { label: "Online", className: "bg-green-500/15 text-green-500 border-green-500/30" },
    connecting: { label: "Connecting…", className: "bg-amber-500/15 text-amber-500 border-amber-500/30" },
    offline: { label: "Offline", className: "bg-slate-500/15 text-slate-400 border-slate-500/30" },
    error: { label: "Error", className: "bg-red-500/15 text-red-500 border-red-500/30" },
  };
  const { label, className } = map[status.state] ?? map.connecting;
  return (
    <span
      title={status.detail || undefined}
      className={`inline-flex items-center gap-1 rounded-full border ml-2 px-2 py-0.5 text-xs font-medium ${className}`}
    >
      <span className="inline-block h-1.5 w-1.5 rounded-full bg-current" />
      <span className="pr-1" />
      {label}
    </span>
  );
}

export default function Dashboard() {
  const { windowQrValues: [qrValues], windowSessionId: [sessionId] } = useContext(GlobalProviderContext);
  const [cloudStatus, setCloudStatus] = useState<CloudStatus>({ state: "connecting", detail: "" });

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    let gotLiveEvent = false;
    void (async () => {
      unlisten = await events.cloudStatusChange.listen((event) => {
        gotLiveEvent = true;
        setCloudStatus(event.payload as CloudStatus);
      });
      try {
        const current = await commands.getCloudStatus();
        if (!cancelled && !gotLiveEvent) setCloudStatus(current as CloudStatus);
      } catch {}
    })();
    return () => { cancelled = true; if (unlisten) unlisten(); };
  }, []);

  const cloudUrl = buildCloudQrValue(sessionId);
  const lanValues = qrValues.filter((qr) => qr.value.length > 0);
  const cloudReady = cloudStatus.state === "registered";
  const cloudBlurredLabel =
    cloudStatus.state === "connecting" ? "Connecting…"
    : cloudStatus.state === "error" ? "Unavailable"
    : "Offline";

  return (
    <Layout>
      <div className="p-8">
        <h2 className="flex justify-center text-4xl font-semibold">What network is your device connected to?</h2>
      </div>
      <div className="w-full overflow-hidden box-border mb-10">
        <div className="px-8 overflow-auto max-w-full mx-auto box-content hidden lg:flex items-center gap-8">
          {cloudUrl && (
            <QrDisplay name="Anywhere (Internet)" url={cloudUrl} badge={<CloudBadge status={cloudStatus} />} blurred={!cloudReady} blurredLabel={cloudBlurredLabel} />
          )}
          {lanValues.length ? (
            lanValues.map((qrValue) => (
              <QrDisplay
                name={qrValue.title}
                url={qrValue.value}
              />
            ))
          ) : !cloudUrl ? (
            <div className="h-[120%] lg:block text-slate-700 dark:text-slate-300 text-lg">
              Join or <b><Link to="/settings" className="underline">Create</Link></b> a Network (none were found)
            </div>
          ) : null}
        </div>
        {cloudUrl || qrValues.length ? (
          <Carousel className="w-full max-w-xs lg:hidden mx-auto" style={{ msOverflowStyle: "none", scrollbarWidth: "none", overflow: "-moz-scrollbars-none", overflowX: "scroll" }} id={"mainCarousel"}>
            <style>{`
              #mainCarousel::-webkit-scrollbar {
                display: none;
                background: transparent;
                width: 0;
                height: 0;
              }
            `}</style>
            <CarouselContent>
              {cloudUrl && (
                <CarouselItem>
                  <QrDisplay name="Anywhere (Internet)" url={cloudUrl} badge={<CloudBadge status={cloudStatus} />} blurred={!cloudReady} blurredLabel={cloudBlurredLabel} />
                </CarouselItem>
              )}
              {lanValues.map((qrValue) => (
                <CarouselItem>
                  <QrDisplay
                    name={qrValue.title}
                    url={qrValue.value}
                  />
                </CarouselItem>
              ))}
            </CarouselContent>
            <CarouselPrevious />
            <CarouselNext />
          </Carousel>
        ) : (
          <div className="text-slate-400 lg:hidden">
            Join or <b><Link to="/settings" className="underline">Create</Link></b> a Network (none were found)
          </div>
        )}
      </div>
    </Layout>
    );
}

const QrDisplay = ({ name, url, badge, blurred, blurredLabel }: { name: string; url: string; badge?: ReactNode; blurred?: boolean; blurredLabel?: string }) => {
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    try {
      await writeText(url);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {}
  };

  return (
    // Definite width (w-96, capped by max-w-full on narrow viewports) so the
    // QR's `width: 100%` resolves against a fixed length. Without this the width
    // is content-derived and the SVG's own intrinsic size (500px) leaks into the
    // calculation, which WebKit resolves differently on an in-place re-render
    // than on a fresh mount — making the QR grow until an HMR remount.
    <div className="p-1 mx-auto w-96 min-w-72 max-w-full">
      <h2 className="text-2xl font-bold text-center mb-2 flex items-center justify-center gap-2">
        {name}
        {badge}
      </h2>
      <Card className="max-w-96 min-w-72 mx-auto w-full p-1 relative overflow-hidden">
        <QRCode
          size={500}
          style={{
            display: "block",
            // Pin the height to the width via aspect-ratio instead of
            // height: "auto". The SVG carries explicit width/height attrs, and
            // WebKit (macOS WKWebView) does not re-resolve its intrinsic aspect
            // ratio on an in-place re-render (e.g. when the cloud status badge
            // flips), so it would grow toward the attribute size until an HMR
            // remount. A fixed aspect-ratio keeps it stable on every render.
            width: "100%",
            maxWidth: "100%",
            height: "auto",
            aspectRatio: "1 / 1",
            borderRadius: "0.275rem",
            transition: "filter 0.2s",
            filter: blurred ? "blur(10px)" : undefined
          }}
          value={url}
          viewBox="0 0 256 256"
        />
        {blurred && (
          <div className="absolute inset-0 flex items-center justify-center z-10">
            <span className="rounded-md bg-black/60 px-3 py-1.5 text-sm font-medium text-white">
              {blurredLabel ?? "Unavailable"}
            </span>
          </div>
        )}
      </Card>
      <Card className="max-w-96 mx-auto w-full p- mt-4 space-y-3 border-none" style={{ filter: blurred ? "blur(10px)" : undefined, pointerEvents: blurred ? "none" : undefined, userSelect: blurred ? "none" : undefined }}>
        <div className="flex items-center justify-between border rounded-md">
          <input
            className="w-full p-2 border-none rounded-md bg-transparent"
            type="text"
            value={url}
            disabled
          />
          <button
            className="p-2 border-l"
            onClick={handleCopy}
            aria-label={copied ? "Copied" : "Copy URL"}
          >
            <span className="relative grid place-items-center" style={{ width: 15, height: 15 }}>
              <Copy
                size={15}
                className={`col-start-1 row-start-1 transition-all duration-200 ${copied ? "scale-50 opacity-0" : "scale-100 opacity-100"}`}
              />
              <Check
                size={15}
                className={`col-start-1 row-start-1 text-green-500 transition-all duration-200 ${copied ? "scale-100 opacity-100" : "scale-50 opacity-0"}`}
              />
            </span>
          </button>
        </div>
        <QrModalComponent value={url} />
      </Card>
    </div>
  );
};

function QrModalComponent({ value }: { value: string }) {
  const [openModal, setOpenModal] = useState(false);

  useEffect(() => {
    if (!openModal) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpenModal(false);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [openModal]);

  return (
    <>
      <Button
        onClick={() => setOpenModal(true)}
        className="w-full"
      >
        Expand QR{" "}
      </Button>
      {openModal && createPortal(
        // Inline styles (not Tailwind classes) for the critical layout props so
        // positioning can't be defeated by class generation or specificity, and
        // the portal escapes any transformed/filtered ancestor that would make
        // `position: fixed` resolve to an element box instead of the viewport.
        <div
          id="mainQRCodeOuter"
          onClick={() => setOpenModal(false)}
          style={{
            position: "fixed",
            top: 0,
            left: 0,
            right: 0,
            bottom: 0,
            zIndex: 9999,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            backgroundColor: "rgba(0, 0, 0, 0.75)",
          }}
        >
          {/*
            Square sized to the smaller viewport side (90vmin) so it stays as
            large as possible with a slight margin, on any window shape. `vmin`
            tracks resizes with no JS. The white background + padding form the
            QR's quiet-zone border against the dark backdrop.
          */}
          <div
            onClick={(event) => event.stopPropagation()}
            style={{
              width: "90vmin",
              height: "90vmin",
              padding: "min(2.5vmin, 1rem)",
              boxSizing: "border-box",
              backgroundColor: "#ffffff",
              borderRadius: "1rem",
            }}
          >
            <QRCode
              id="mainQRCode"
              value={value}
              viewBox="0 0 256 256"
              style={{ width: "100%", height: "100%", display: "block" }}
            />
          </div>
        </div>,
        document.body
      )}
    </>
    );
}
