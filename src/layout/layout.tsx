import React, { useState, useEffect, useContext } from "react";

import { Sidebar } from "./sidebar";
import { ModeToggle } from "@/components/mode-toggle";
import { ProfileMenu } from "@/components/profile-menu";
import { AlignLeft } from "lucide-react";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";

import { AuthProviderContext } from "@/components/auth-provider";
import { listen } from "@tauri-apps/api/event";

export default function Layout({ children }: { children: React.ReactNode }) {
  const { currentUser } = useContext(AuthProviderContext);

  if (localStorage.getItem(currentUser.username + "-isSideBarOpen") === null) {
    localStorage.setItem(currentUser.username + "-isSideBarOpen", "true");
  }

  const [isSideBarOpen, setIsSideBarOpen] = useState(JSON.parse(localStorage.getItem(currentUser.username + "-isSideBarOpen")!));
  const [firstTime, setFirstTime] = useState(true);
  const [doneOpening, setDoneOpening] = useState(false);

  const [minSize, setMinSize] = useState(27500/window.innerWidth);
//  const [maxSize, setMaxSize] = useState(40000/window.innerWidth);
  void listen<string>("tauri://resize", () => {
    if (parseFloat(document.getElementById("sideBar")!.style.flexGrow) > 0) {
      setMinSize(27500/window.innerWidth);
//      setMaxSize(40000/window.innerWidth);
    }
  });

//  const arrow = document.getElementById("hideArrow")!;
//  const innerSideBar = document.getElementById("innerSidebar")!;
//  void listen<string>("tauri://resize", () => {
//    if (arrow.getBoundingClientRect().width > 0) {
//      setIsSideBarOpen(false);
//      innerSideBar.style.display = "none";
//    }
//  });

  const [defaultSize, setDefaultSize] = useState(parseFloat(localStorage.getItem(currentUser.username + "-defaultSize") || 27500/window.innerWidth + ""));
  const [previousDefaultSize, setPreviousDefaultSize] = useState(defaultSize);
  localStorage.setItem(currentUser.username + "-defaultSize", defaultSize.toString());
  useEffect(() => {
    localStorage.setItem(currentUser.username + "-defaultSize", defaultSize.toString());
  }, [defaultSize]);

  useEffect(() => {
    localStorage.setItem(currentUser.username + "-isSideBarOpen", isSideBarOpen.toString());
    const sideBar = document.getElementById("sideBar")!;
    if (!isSideBarOpen) {
      setPreviousDefaultSize(defaultSize);
      sideBar.animate(
        [
          { flexGrow: defaultSize },
          { flexGrow: 0 }
        ],
        {
          duration: firstTime ? 0 : 250
        }
      );
      sideBar.style.flexGrow = "0";
      setDefaultSize(0);
      setDoneOpening(true);
    } else if (doneOpening) {
      sideBar.animate(
        [
          { flexGrow: 0 },
          { flexGrow: previousDefaultSize }
        ],
        {
          duration: 250
        }
      );
      sideBar.style.flexGrow = previousDefaultSize.toString();
      setDefaultSize(previousDefaultSize);
      setDoneOpening(false);
    }
    setFirstTime(false);
  }, [isSideBarOpen]);

  return (
    <ResizablePanelGroup
      className="flex h-screen"
      direction="horizontal"
    >
      <ResizablePanel
        minSize={minSize}
        maxSize={minSize} // maxSize
        defaultSize={defaultSize}
        id="sideBar"
        onResize={(width) => setDefaultSize(width)}
      >
        <Sidebar />
      </ResizablePanel>
      <ResizableHandle style={(isSideBarOpen ? {} : {display: "none"})} disabled={true} /> {/* withHandle */}
      <ResizablePanel>
        <div className="flex-1 h-screen flex flex-col overflow-hidden">
          <div className="flex items-center justify-between px-4 py-2 border-b">
            <div
              className="cursor-pointer"
              onClick={() => setIsSideBarOpen((prev: boolean) => !prev)}
            >
              <AlignLeft size={24} />
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
