import React, { useState, useContext } from "react";

import { Sidebar } from "@/layout/sidebar";
import { ModeToggle } from "@/components/mode-toggle";
import { ProfileMenu } from "@/components/profile-menu";
import { PanelLeftClose, PanelLeftOpen } from "lucide-react";

import { GlobalProviderContext } from "@/components/global-provider";

const SIDEBAR_WIDTH = 275;

export default function Layout({ children }: { children: React.ReactNode }) {
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const { windowOtp: [otp] } = useContext(GlobalProviderContext);

  return (
    <div className="flex h-screen w-full">
      <div
        id="sidebar"
        className="relative h-screen shrink-0 overflow-hidden transition-[width] duration-300 ease-in-out"
        style={{ width: sidebarOpen ? SIDEBAR_WIDTH : 0 }}
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
          <ProfileMenu />
        </div>
        <div className="h-full flex-grow overflow-y-auto w-full overflow-hidden">
          {children}
        </div>
        <div className="flex items-center justify-end p-4 border-t">
          <div className="flex items-center space-x-2 mx-2 text-lg">
            <p>Session OTP: {otp}</p>
          </div>
          <div className="flex-1"></div>
          <div className="flex items-center space-x-2">
            <ModeToggle />
          </div>
        </div>
      </div>
    </div>
  );
}
