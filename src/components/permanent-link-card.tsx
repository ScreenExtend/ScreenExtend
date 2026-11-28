import { useEffect, useState } from "react";
import QRCode from "react-qr-code";
import { Copy, Link2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { toast } from "@/components/ui/use-toast";
import { flushConfig, updateConfig, useConfig, DEFAULT_HTTP_PORT, type Config } from "@/components/config-provider";
import { useTranslation } from "@/i18n";
import { commands } from "@/lib/bindings";
import { buildQrValues, generateOtp, type QrEntry } from "@/lib/utils";

type Link = Config["permanentLink"];

const OFF: Link = { enabled: false, sessionId: "", otp: "" };

// the client page only accepts ids from this alphabet, 12 long
function generateSessionId(): string {
  const alphabet = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
  return Array.from(crypto.getRandomValues(new Uint8Array(12)), b => alphabet[b % alphabet.length]).join("");
}

export function PermanentLinkCard() {
  const { t } = useTranslation();
  const config = useConfig();
  const link: Link = config?.permanentLink ?? OFF;
  const httpPort = config?.serverPorts?.http ?? DEFAULT_HTTP_PORT;
  const [entries, setEntries] = useState<QrEntry[]>([]);
  const [shown, setShown] = useState(0);
  const [busy, setBusy] = useState(false);
  const [confirmRegenerate, setConfirmRegenerate] = useState(false);

  useEffect(() => {
    if (!link.enabled || !link.sessionId) {
      setEntries([]);
      return;
    }
    let live = true;
    buildQrValues(link.sessionId, httpPort)
      .then(found => {
        if (!live) return;
        setEntries(found.map(e => ({ ...e, value: `${e.value}&otp=${link.otp}` })));
        setShown(0);
      })
      .catch(() => { if (live) setEntries([]); });
    return () => { live = false; };
  }, [link.enabled, link.sessionId, link.otp, httpPort]);

  const save = async (next: Link) => {
    setBusy(true);
    try {
      const result = await commands.setPermanentLink(next.enabled ? next.sessionId : null, next.enabled ? next.otp : null);
      if (result.status === "error") throw new Error(result.error);
      await updateConfig({ permanentLink: next });
      await flushConfig();
    } catch (e) {
      console.error("saving the permanent link failed", e);
      toast({
        variant: "destructive",
        title: t("permanentLink.failedTitle"),
        description: t("permanentLink.failedDescription"),
      });
    } finally {
      setBusy(false);
    }
  };

  const generate = () => save({ enabled: true, sessionId: generateSessionId(), otp: generateOtp(6) });
  // turning it off forgets the pair, so turning it on again never revives an old link
  const turnOff = () => save(OFF);

  const copy = async (value: string) => {
    try {
      await navigator.clipboard.writeText(value);
      toast({ title: t("permanentLink.copied") });
    } catch (e) {
      console.error("copying the permanent link failed", e);
    }
  };

  const current = entries[shown];

  return (
    <Card>
      <CardHeader>
        <div className="flex flex-row items-start">
          <div className="flex-1">
            <CardTitle className="flex flex-row items-center">
              <Link2 size={18} className="mr-2" />
              {t("permanentLink.title")}
            </CardTitle>
            <p className="text-sm text-muted-foreground mt-1">{t("permanentLink.description")}</p>
          </div>
          <Switch
            checked={link.enabled}
            disabled={busy}
            onCheckedChange={on => void (on ? generate() : turnOff())}
            aria-label={t("permanentLink.title")}
          />
        </div>
      </CardHeader>
      {link.enabled && (
        <CardContent>
          <p className="text-sm text-destructive mb-3">{t("permanentLink.warning")}</p>
          {entries.length === 0 ? (
            <p className="text-sm text-muted-foreground">{t("permanentLink.noNetwork")}</p>
          ) : (
            <div className="flex flex-row items-start">
              {current && (
                <div className="bg-white p-2 rounded mr-4 shrink-0">
                  <QRCode value={current.value} size={128} />
                </div>
              )}
              <div className="flex-1 min-w-0">
                {entries.map((entry, i) => (
                  <div key={entry.value} className="flex flex-row items-center mb-2">
                    <button
                      type="button"
                      className={"flex-1 min-w-0 text-left rounded px-2 py-1 mr-2 " + (i === shown ? "bg-muted" : "")}
                      onClick={() => setShown(i)}
                      title={t("permanentLink.showQr")}
                    >
                      <div className="text-xs text-muted-foreground">{entry.title}</div>
                      <div className="text-sm font-mono truncate">{entry.value}</div>
                    </button>
                    <Button variant="outline" size="sm" onClick={() => void copy(entry.value)}>
                      <Copy size={14} className="mr-1" />
                      {t("permanentLink.copy")}
                    </Button>
                  </div>
                ))}
              </div>
            </div>
          )}
          <div className="flex flex-row mt-3">
            <Button variant="secondary" size="sm" className="mr-2" disabled={busy} onClick={() => setConfirmRegenerate(true)}>
              {t("permanentLink.regenerate")}
            </Button>
          </div>
        </CardContent>
      )}
      <AlertDialog open={confirmRegenerate} onOpenChange={setConfirmRegenerate}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("permanentLink.regenerateTitle")}</AlertDialogTitle>
            <AlertDialogDescription>{t("permanentLink.regenerateDescription")}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>{t("permanentLink.cancel")}</AlertDialogCancel>
            <AlertDialogAction onClick={() => void generate()}>{t("permanentLink.regenerate")}</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Card>
  );
}
