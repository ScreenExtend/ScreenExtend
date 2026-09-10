import { useEffect, useState } from "react";
import { Info } from "lucide-react";

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

// The other half of the macOS TCC watchdog. It re-reads `screen_recording_live()` every
// 30 s while a session holds a live display and ends the sessions on a definite
// granted -> denied edge -- macOS 15 re-asks for consent roughly every 30 days, and a
// dismissed dialog does *not* stop `CGDisplayStream`, which keeps delivering frames of the
// wallpaper. Until now that edge was named in the log and nowhere else, so the person at
// the host saw a device drop with no reason given. This raises the permission gate over
// whatever route is mounted, with the copy that says a relaunch is part of the fix.
//
// Nothing emits the event off macOS.
export function PermissionLapseDialog() {
  const { t } = useTranslation();
  const [lapse, setLapse] = useState<{ key: string; sessionsEnded: number } | null>(null);

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

  // Poll while the dialog is up so ticking the box in System Settings closes it, the same
  // way the bootstrap gate behaves. A grant bound to the old process still reads as denied
  // here, which is what the relaunch button is for.
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
              // The dialog re-opens on the next revocation; dismissing is not a grant.
              event.preventDefault();
              setLapse(null);
            }}
          >
            {t("bootstrap.permissions.dismiss")}
          </AlertDialogAction>
          <AlertDialogAction
            className="bg-secondary hover:bg-secondary/80 text-secondary-foreground"
            onClick={event => {
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
              // Through LaunchServices, so the new process is its own TCC subject rather
              // than an heir of this one -- the same reason bootstrap uses it.
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
