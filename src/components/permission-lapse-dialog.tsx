import { useEffect, useRef, useState } from "react";
import { Info, Loader2 } from "lucide-react";

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { toast } from "@/components/ui/use-toast";
import { useTranslation } from "@/i18n";
import { commands, events } from "@/lib/bindings";

const stopListening = (unlisten: () => void) => {
  void Promise.resolve(unlisten()).catch(e =>
    console.error("screenRecordingRevoked unlisten failed", e)
  );
};

// Raised when the host's macOS screen-recording watchdog ends the sessions on a definite
// denial (without the permission CGDisplayStream keeps delivering frames of the wallpaper).
// Shown over whatever route is mounted. Nothing emits the event off macOS.
export function PermissionLapseDialog() {
  const { t } = useTranslation();
  const [lapse, setLapse] = useState<{ key: string; sessionsEnded: number } | null>(null);
  // one Retry at a time: each may raise the macOS dialog and clear the TCC entry, so a double
  // click must not send two
  const [retrying, setRetrying] = useState(false);
  const retryInFlight = useRef(false);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    void events.screenRecordingRevoked
      .listen(event => {
        const payload = event.payload as { key: string; sessions_ended: number };
        setLapse({ key: payload.key, sessionsEnded: payload.sessions_ended });
      })
      .then(fn => {
        if (disposed) stopListening(fn);
        else unlisten = fn;
      })
      .catch(e => console.error("screenRecordingRevoked.listen failed", e));
    return () => {
      disposed = true;
      if (unlisten) stopListening(unlisten);
    };
  }, []);

  // poll so ticking the box in System Settings closes the dialog. A grant bound to the old
  // process still reads denied (hence Relaunch). checkPermissions is a silent read: only
  // Retry may raise the macOS prompt.
  useEffect(() => {
    if (!lapse) return;
    let cancelled = false;
    const timer = window.setInterval(() => {
      commands
        .checkPermissions()
        .then(perms => {
          if (cancelled) return;
          const still = perms.find(p => p.key === lapse.key);
          if (still && still.state === "granted") setLapse(null);
        })
        .catch(e => console.error("re-checking permissions after a revocation failed", e));
    }, 3000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [lapse]);

  if (!lapse) return null;

  return (
    <AlertDialog open>
      <AlertDialogContent className="max-w-xl">
        <AlertDialogHeader>
          <AlertDialogTitle>{t("bootstrap.permissions.revokedTitle")}</AlertDialogTitle>
          <AlertDialogDescription asChild>
            <div className="space-y-3 text-left">
              <div>
                {lapse.sessionsEnded > 0
                  ? t("bootstrap.permissions.revokedBodyWithCount", { count: lapse.sessionsEnded })
                  : t("bootstrap.permissions.revokedBody")}
              </div>
              <div className="flex items-start space-x-2 rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
                <Info className="mt-0.5 shrink-0 text-amber-600" size={16} />
                <div>{t("bootstrap.permissions.revokedSteps")}</div>
              </div>
            </div>
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogAction
            className="mr-auto bg-secondary hover:bg-secondary/80 text-secondary-foreground"
            onClick={event => {
              // dismissing is not a grant: the dialog re-opens on the next revocation
              event.preventDefault();
              setLapse(null);
            }}
          >
            {t("bootstrap.permissions.dismiss")}
          </AlertDialogAction>
          <AlertDialogAction
            className="bg-secondary hover:bg-secondary/80 text-secondary-foreground"
            title={t("bootstrap.permissions.retryHint")}
            disabled={retrying}
            onClick={event => {
              // "retry:" marks a user-pressed Retry (RETRY_PREFIX in macos_utils/permissions.rs);
              // without it the request is a silent read, since this permission was asked before
              event.preventDefault();
              // the ref covers the gap before React re-renders the button disabled
              if (retryInFlight.current) return;
              retryInFlight.current = true;
              setRetrying(true);
              commands
                .requestPermission(`retry:${lapse.key}`)
                .catch((e: unknown) => {
                  console.error("retrying the permission failed", e);
                })
                .finally(() => {
                  retryInFlight.current = false;
                  setRetrying(false);
                });
            }}
          >
            {retrying ? <Loader2 className="animate-spin" size={14} /> : t("bootstrap.permissions.retry")}
          </AlertDialogAction>
          <AlertDialogAction
            className="bg-secondary hover:bg-secondary/80 text-secondary-foreground"
            onClick={event => {
              // opens the pane only, never requests
              event.preventDefault();
              commands.openPermissionSettings(lapse.key).catch((e: unknown) => {
                console.error("openPermissionSettings failed", e);
              });
            }}
          >
            {t("bootstrap.permissions.openSettings")}
          </AlertDialogAction>
          <AlertDialogAction
            className="bg-blue-600 hover:bg-blue-700 text-white"
            onClick={() => {
              // through LaunchServices, so the new process is its own TCC subject, not an heir
              // of this one
              commands.relaunchApp().catch(() => {
                toast({
                  variant: "destructive",
                  title: t("toasts.relaunch.failureTitle"),
                  description: t("toasts.relaunch.failureDescription"),
                });
              });
            }}
          >
            {t("bootstrap.permissions.relaunch")}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
