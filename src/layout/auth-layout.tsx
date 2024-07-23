import React from "react";

import { ModeToggle } from "@/components/mode-toggle";

import { ReactSVG } from "react-svg";

const AuthLayout = ({ children }: { children: React.ReactNode }) => {
  return (
    <div className="container relative h-screen flex-col items-center justify-start md:grid lg:max-w-none lg:grid-cols-2 md:grid-cols-1 lg:px-0">
      <div className="absolute right-4 top-4 md:right-8 md:top-8">
        <ModeToggle />
      </div>
      <div className="relative hidden h-full flex-col bg-muted p-10 text-white lg:flex dark:border-r">
        <div className="absolute inset-0 bg-blue-800 flex items-center justify-center">
          <div className="text-center">
            <ReactSVG src="/src/assets/illustration.svg" className="object-cover" style={{ width: "28rem" }} />
            <div>
              <h1 className="text-3xl font-bold" style={{ maxWidth: "26rem" }}>
                ScreenExtend: The easiest way to extend your screen.
              </h1>
              <p className="text-lg mt-4">
                Extend your screen. Extend your possibilities.
              </p>
            </div>
          </div>
        </div>
        <div className="relative z-20 flex items-center text-lg font-medium text-blue-200">
          <ReactSVG src="/src/assets/logo.svg" className="object-cover" style={{ width: "2rem", marginRight: "10px" }} />
          ScreenExtend
        </div>
      </div>
      <div className="lg:p-8">{children}</div>
    </div>
  );
};

export default AuthLayout;
