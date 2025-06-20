import React, { useState, useEffect } from "react";

import { Sidebar } from "@/layout/sidebar";
import { ModeToggle } from "@/components/mode-toggle";
import { ProfileMenu } from "@/components/profile-menu";
import { PanelLeftClose, PanelLeftOpen } from "lucide-react";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";

import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
const appWindow = getCurrentWebviewWindow();

export default function Layout({ children }: { children: React.ReactNode }) {
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [doneOpening, setDoneOpening] = useState(false);

  const [sidebarSize, setSidebarSize] = useState(27500/window.innerWidth);
  void appWindow.onResized(({ payload: size }) => {
    if (parseFloat(document.getElementById("sidebar")!.style.flexGrow) > 0) {
      setSidebarSize(27500/size.width);
    }
  });

  useEffect(() => {
    const sidebar = document.getElementById("sidebar")!;
    if (!sidebarOpen) {
      sidebar.animate(
        [
          { flexGrow: sidebarSize },
          { flexGrow: 0 }
        ],
        {
          duration: 250
        }
      );
      sidebar.style.flexGrow = "0";
      setDoneOpening(true);
    } else if (doneOpening) {
      sidebar.animate(
        [
          { flexGrow: 0 },
          { flexGrow: 27500/window.innerWidth }
        ],
        {
          duration: 250
        }
      );
      setSidebarSize(27500/window.innerWidth);
      sidebar.style.flexGrow = sidebarSize.toString();
      setDoneOpening(false);
    }
  }, [sidebarOpen]);

  return (
    <ResizablePanelGroup
      className="flex h-screen"
      direction="horizontal"
    >
      <ResizablePanel
        minSize={sidebarSize}
        maxSize={sidebarSize}
        defaultSize={sidebarSize}
        id="sidebar"
      >
        <Sidebar />
      </ResizablePanel>
      <ResizableHandle style={(sidebarOpen ? {} : {display: "none"})} disabled={true} /> {/* withHandle */}
      <ResizablePanel>
        <div className="flex-1 h-screen flex flex-col overflow-hidden">
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
            <div className="flex items-center space-x-2">
              <ModeToggle />
            </div>
          </div>
        </div>
      </ResizablePanel>
    </ResizablePanelGroup>
  );
}
