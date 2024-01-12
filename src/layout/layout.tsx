import React, { useState } from "react";
import { Sidebar } from "./sidebar";
import { Avatar, AvatarFallback, AvatarImage } from "@/components/ui/avatar";
import { AlignLeft } from "lucide-react";
import { cn } from "@/lib/utils";
import { ModeToggle } from "@/components/mode-toggle";

export default function Layout({ children }: { children: React.ReactNode }) {
  const [isSideBarOpen, setIsSideBarOpen] = useState(false);

  return (
    <div className="flex h-screen">
      <Sidebar
        setIsSideBarOpen={setIsSideBarOpen}
        className={cn(
          "absolute bg-white dark:bg-background z-10 border-r lg:border-r-0 lg:relative h-screen lg:block transition-all duration-300 max-w-full",
          isSideBarOpen ? "-left-96 fixed w-0" : "left-0 fixed w-96"
        )}
      />
      <div className="flex-1 h-screen flex flex-col">
        <div className="flex items-center justify-between px-4 py-2 border-b">
          <div
            className="cursor-pointer"
            onClick={() => setIsSideBarOpen((prev) => !prev)}
          >
            <AlignLeft size={24} className="" />
          </div>
          <Avatar>
            <AvatarImage src="https://github.com/shadcn.png" alt="@shadcn" />
            <AvatarFallback>CN</AvatarFallback>
          </Avatar>
        </div>
        <div className="h-full flex-grow overflow-y-auto">{children}</div>
        <div className="flex items-center justify-end p-4 border-t">
          <div className="flex items-center space-x-2">
            <ModeToggle />
          </div>
        </div>
      </div>
    </div>
  );
}
