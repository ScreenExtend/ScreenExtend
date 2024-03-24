import React, { useState, useEffect, useContext } from "react";
import { Sidebar } from "./sidebar";
import { AlignLeft } from "lucide-react";
import { ModeToggle } from "@/components/mode-toggle";
import { ProfileMenu } from "@/components/profile-menu";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { AuthProviderContext } from "@/components/auth-provider";

export default function Layout({ children }: { children: React.ReactNode }) {
  // @ts-ignore
  const { currentUser } = useContext(AuthProviderContext);

  // @ts-ignore
  if (JSON.parse(localStorage.getItem(currentUser.username + "-isSideBarOpen")) === null) {
    localStorage.setItem(currentUser.username + "-isSideBarOpen", "true");
  }

  // @ts-ignore
  const [isSideBarOpen, setIsSideBarOpen] = useState(JSON.parse(localStorage.getItem(currentUser.username + "-isSideBarOpen")));
  const [firstTime, setFirstTime] = useState(true);
  const [doneOpening, setDoneOpening] = useState(false);

  const [minSize, setMinSize] = useState(27500/window.innerWidth);
  const [maxSize, setMaxSize] = useState(40000/window.innerWidth);
  window.addEventListener("resize", function() {
    setMinSize(27500/window.innerWidth);
    setMaxSize(40000/window.innerWidth);
  }, true);

  // @ts-ignore
  const [defaultSize, setDefaultSize] = useState(parseFloat(localStorage.getItem(currentUser.username + "-defaultSize")) || 27500/window.innerWidth);
  localStorage.setItem(currentUser.username + "-defaultSize", defaultSize.toString());
  useEffect(() => {
    localStorage.setItem(currentUser.username + "-defaultSize", defaultSize.toString());
  }, [defaultSize]);

  useEffect(() => {
    localStorage.setItem(currentUser.username + "-isSideBarOpen", isSideBarOpen.toString());
    if (document.getElementById("sideBar")) {
      if (!isSideBarOpen) {
        document.getElementById("sideBar")?.animate(
          [
            { flexGrow: defaultSize },
            { flexGrow: 0 }
          ],
          {
            duration: firstTime ? 0 : 250
          }
        );
        // @ts-ignore
        document.getElementById("sideBar").style.flexGrow = "0";
        setDoneOpening(true);
      } else if (doneOpening) {
        document.getElementById("sideBar")?.animate(
          [
            { flexGrow: 0 },
            { flexGrow: defaultSize }
          ],
          {
            duration: 250
          }
        );
        // @ts-ignore
        document.getElementById("sideBar").style.flexGrow = defaultSize.toString();
        setDoneOpening(false);
      }
    }
    setFirstTime(false);
  }, [isSideBarOpen]);

  return (
    <ResizablePanelGroup className="flex h-screen" direction={"horizontal"}>
      <ResizablePanel minSize={minSize} maxSize={maxSize} defaultSize={defaultSize} id={"sideBar"} onResize={(width) => { setDefaultSize(width);console.log(width) }}>
        <Sidebar
          setIsSideBarOpen={console.log}
          className={ "absolute bg-white dark:bg-background z-10 border-r lg:border-r-0 lg:relative h-screen lg:block transition-all duration-500 max-w-full left-0" }
        />
      </ResizablePanel>
      <ResizableHandle withHandle style={(isSideBarOpen ? {} : {display: "none"})} />
      <ResizablePanel>
        <div className="flex-1 h-screen flex flex-col overflow-hidden">
          <div className="flex items-center justify-between px-4 py-2 border-b">
            <div
              className="cursor-pointer"
              onClick={() => setIsSideBarOpen((prev: boolean) => !prev)}
              >
              <AlignLeft size={24} className="" />
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
