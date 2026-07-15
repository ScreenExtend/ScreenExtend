import { useCallback, useEffect, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { Slider } from "@/components/ui/slider";
import { AVATAR_OUTPUT_SIZE } from "@/lib/avatar";

const OUTPUT = AVATAR_OUTPUT_SIZE;
const MAX_ZOOM = 4;

type AvatarCropModalProps = {
  open: boolean;
  imageSrc: string | null;
  onCancel: () => void;
  onSave: (bytes: Uint8Array, dataUrl: string) => void | Promise<void>;
};

export function AvatarCropModal({ open, imageSrc, onCancel, onSave }: AvatarCropModalProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const imgRef = useRef<HTMLImageElement | null>(null);
  const coverRef = useRef(1);
  const scaleRef = useRef(1);
  const offsetRef = useRef({ x: 0, y: 0 });
  const dragging = useRef(false);
  const last = useRef({ x: 0, y: 0 });

  const [zoom, setZoom] = useState(1);
  const [ready, setReady] = useState(false);
  const [saving, setSaving] = useState(false);

  const clampOffset = useCallback((scale: number) => {
    const img = imgRef.current;
    if (!img) return;
    const w = img.naturalWidth * scale;
    const h = img.naturalHeight * scale;
    offsetRef.current.x = Math.min(0, Math.max(OUTPUT - w, offsetRef.current.x));
    offsetRef.current.y = Math.min(0, Math.max(OUTPUT - h, offsetRef.current.y));
  }, []);

  const draw = useCallback((scale: number) => {
    const canvas = canvasRef.current;
    const img = imgRef.current;
    if (!canvas || !img) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    clampOffset(scale);
    ctx.clearRect(0, 0, OUTPUT, OUTPUT);
    ctx.imageSmoothingQuality = "high";
    ctx.drawImage(
      img,
      offsetRef.current.x,
      offsetRef.current.y,
      img.naturalWidth * scale,
      img.naturalHeight * scale
    );
  }, [clampOffset]);

  useEffect(() => {
    if (!open || !imageSrc) return;
    setReady(false);
    const img = new Image();
    img.onload = () => {
      imgRef.current = img;
      const cover = OUTPUT / Math.min(img.naturalWidth, img.naturalHeight);
      coverRef.current = cover;
      scaleRef.current = cover;
      offsetRef.current = {
        x: (OUTPUT - img.naturalWidth * cover) / 2,
        y: (OUTPUT - img.naturalHeight * cover) / 2,
      };
      setZoom(1);
      setReady(true);
      draw(cover);
    };
    img.src = imageSrc;
    return () => {
      img.onload = null;
    };
  }, [open, imageSrc, draw]);

  const applyZoom = useCallback((nextZoom: number) => {
    const oldScale = scaleRef.current;
    const newScale = coverRef.current * nextZoom;
    const c = OUTPUT / 2;
    const imgX = (c - offsetRef.current.x) / oldScale;
    const imgY = (c - offsetRef.current.y) / oldScale;
    offsetRef.current.x = c - imgX * newScale;
    offsetRef.current.y = c - imgY * newScale;
    scaleRef.current = newScale;
    setZoom(nextZoom);
    draw(newScale);
  }, [draw]);

  const onPointerDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!ready) return;
    dragging.current = true;
    last.current = { x: e.clientX, y: e.clientY };
    e.currentTarget.setPointerCapture(e.pointerId);
  };

  const onPointerMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!dragging.current) return;
    const rect = e.currentTarget.getBoundingClientRect();
    const ratio = OUTPUT / rect.width; // canvas px per CSS px
    offsetRef.current.x += (e.clientX - last.current.x) * ratio;
    offsetRef.current.y += (e.clientY - last.current.y) * ratio;
    last.current = { x: e.clientX, y: e.clientY };
    draw(scaleRef.current);
  };

  const endDrag = (e: React.PointerEvent<HTMLCanvasElement>) => {
    dragging.current = false;
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
  };

  const handleSave = async () => {
    const canvas = canvasRef.current;
    if (!canvas || saving) return;
    setSaving(true);
    try {
      const blob = await new Promise<Blob | null>((resolve) =>
        canvas.toBlob(resolve, "image/png")
      );
      if (!blob) return;
      const bytes = new Uint8Array(await blob.arrayBuffer());
      await onSave(bytes, canvas.toDataURL("image/png"));
    } finally {
      setSaving(false);
    }
  };

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-[100000] flex items-center justify-center bg-black/80 p-4"
      role="dialog"
      aria-modal="true"
    >
      <div className="w-full max-w-sm rounded-lg border bg-background p-6 shadow-lg">
        <h3 className="text-lg font-semibold">Adjust your profile picture</h3>
        <p className="mt-1 text-sm text-muted-foreground">
          Drag to reposition and use the slider to zoom.
        </p>
        <div className="mt-4 flex justify-center">
          <div
            className="relative overflow-hidden rounded-lg border bg-muted"
            style={{ width: OUTPUT, height: OUTPUT }}
          >
            <canvas
              ref={canvasRef}
              width={OUTPUT}
              height={OUTPUT}
              onPointerDown={onPointerDown}
              onPointerMove={onPointerMove}
              onPointerUp={endDrag}
              onPointerCancel={endDrag}
              className="absolute inset-0 h-full w-full cursor-grab touch-none active:cursor-grabbing"
            />
            <div
              aria-hidden="true"
              className="pointer-events-none absolute inset-0 rounded-full"
              style={{ boxShadow: "0 0 0 9999px rgba(0,0,0,0.45)" }}
            />
          </div>
        </div>
        <div className="mt-5 flex items-center gap-3">
          <span className="text-xs text-muted-foreground">Zoom</span>
          <Slider
            value={[zoom]}
            min={1}
            max={MAX_ZOOM}
            step={0.01}
            disabled={!ready}
            onValueChange={([value]) => applyZoom(value)}
          />
        </div>
        <div className="mt-6 flex justify-end gap-2">
          <Button variant="outline" onClick={onCancel} disabled={saving}>
            Cancel
          </Button>
          <Button onClick={() => void handleSave()} disabled={!ready || saving}>
            {saving ? "Saving…" : "Save Photo"}
          </Button>
        </div>
      </div>
    </div>
  );
}
