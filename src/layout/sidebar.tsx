import { Button } from "@/components/ui/button";
import {
  ChevronLeft,
  UserRoundPlus,
  Info,
  MonitorSmartphone,
  Settings,
} from "lucide-react";
import { Link, useLocation } from "react-router-dom";

interface SidebarProps extends React.HTMLAttributes<HTMLDivElement> {
  setIsSideBarOpen: React.Dispatch<React.SetStateAction<boolean>>;
}

export function Sidebar({ className, setIsSideBarOpen }: SidebarProps) {
  const location = useLocation();

  return (
    <div className={className}>
      <div className="space-y-4 h-full flex flex-col w-full border">
        <div className="px-3 py-2 pt-0 h-full">
          <div className="flex items-center justify-between mb-2 px-4 pr-0">
            <h2 className="text-4xl font-medium tracking-tight py-4" style={{marginLeft: "-0.5rem"}}>
              ScreenExtend
            </h2>
            <ChevronLeft
              size={20}
              className="lg:hidden cursor-pointer"
              onClick={() => setIsSideBarOpen((prev) => !prev)}
            />
          </div>
          <div className="space-y-1">
            <Link className="block" to="/dashboard">
              <Button
                variant={
                  !["/settings", "/devices"].includes(location.pathname) &&
                  location.pathname !== "/"
                    ? "secondary"
                    : "ghost"
                }
                className="w-full justify-start text-base py-6"
              >
                <UserRoundPlus size={15} className="mr-2 h-6 w-6" />
                Add Device
              </Button>
            </Link>
            <Link className="block" to="/devices">
              <Button
                variant={
                  location.pathname === "/devices" ? "secondary" : "ghost"
                }
                className="w-full justify-start text-base py-6"
              >
                <MonitorSmartphone size={15} className="mr-2 h-6 w-6" />
                Edit Device
              </Button>
            </Link>
            <Link className="block" to="/settings">
              <Button
                variant={
                  location.pathname === "/settings" ? "secondary" : "ghost"
                }
                className="w-full justify-start text-base py-6"
              >
                <Settings size={15} className="mr-2 h-6 w-6" />
                Settings
              </Button>
            </Link>
          </div>
        </div>
        <div className="flex items-center justify-center gap-1 py-4">
          <Info size={15} />
          <Link to={"/terms"} className="underline">
            Terms and Conditions
          </Link>
        </div>
      </div>
    </div>
  );
}
