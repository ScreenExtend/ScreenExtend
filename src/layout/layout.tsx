import React, { useState } from "react";

import { Sidebar } from "@/layout/sidebar";
import { ModeToggle } from "@/components/mode-toggle";
import { ProfileMenu } from "@/components/profile-menu";
import { PanelLeftClose, PanelLeftOpen } from "lucide-react";

import { useSession } from "@/components/global-provider";

const SIDEBAR_WIDTH = 275;

export default function Layout({ children }: { children: React.ReactNode }) {
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const { otp, sessionId } = useSession();

  return (
    <div className="flex h-screen w-full overflow-hidden">
      {/* This used to transition `width` from 275 to 0 over 300 ms. `width` is a layout
          property and the sibling is the whole page -- the QR carousel, the device table,
          the settings form -- so every frame of the collapse reflowed all of it on the
          main thread of a process that is simultaneously capturing, scaling and encoding
          a display. `transform` and `opacity` are composited: the panel's own subtree
          never re-lays-out, and the reserved 275 px flips once per toggle rather than on
          every frame, so the page below reflows once instead of ~18 times.

          The panel slides rather than clips, which is a different animation from the old
          one and the reason this was held back until it was signed off. `overflow-visible`
          plus `z-10` are what let it slide across the content pane while the space is
          already given back. */}
      <div
        id="sidebar"
        className="relative z-10 h-screen shrink-0 transition-[transform,opacity] duration-300 ease-in-out"
        style={{
          width: sidebarOpen ? SIDEBAR_WIDTH : 0,
          transform: sidebarOpen ? "translateX(0)" : `translateX(-${SIDEBAR_WIDTH}px)`,
          opacity: sidebarOpen ? 1 : 0,
        }}
      >
        <div className="h-full" style={{ width: SIDEBAR_WIDTH }}>
          <Sidebar />
        </div>
      </div>
      <div className="flex-1 min-w-0 h-screen flex flex-col overflow-hidden">
        <div className="flex items-center justify-between px-3 py-3 border-b">
          <div
            className="cursor-pointer px-1"
            onClick={() => setSidebarOpen(prev => !prev)}
          >
            {
              sidebarOpen ? (
                <PanelLeftClose size={24} />
              ) : (
                <PanelLeftOpen size={24} />
              )
            }
          </div>
          <div id="tour-profile">
            <ProfileMenu />
          </div>
        </div>
        <div className="h-full flex-grow overflow-y-auto w-full overflow-hidden">
          {children}
        </div>
        <div className="flex items-center justify-end p-4 border-t">
          <div id="tour-session" className="flex flex-col mx-2 text-sm">
            <p>Session ID: {sessionId}</p>
            <p>Session OTP: {otp}</p>
          </div>
          <div className="flex-1"></div>
          <div id="tour-theme" className="flex items-center space-x-2">
            <ModeToggle />
          </div>
        </div>
      </div>
    </div>
  );
}
