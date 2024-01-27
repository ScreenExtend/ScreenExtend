import { cn } from "@/lib/utils";
import illustration from "@/assets/illustration.svg";
import { ModeToggle } from "@/components/mode-toggle";

const AuthLayout = ({ children }: { children: React.ReactNode }) => {
  return (
    <>
      <div className="container relative h-screen flex-col items-center justify-start md:grid lg:max-w-none lg:grid-cols-2 md:grid-cols-1 lg:px-0">
        <div className={cn("absolute right-4 top-4 md:right-8 md:top-8")}>
          <ModeToggle />
        </div>
        <div className="relative hidden h-full flex-col bg-muted p-10 text-white lg:flex dark:border-r">
          <div className="absolute inset-0 bg-[#00120B] flex items-center justify-center">
            <div className="text-center">
              <img
                src={illustration}
                alt="Illustration"
                // Position the image to the center of the container
                className="object-cover w-96"
              />
              <div>
                <h1 className="text-3xl font-bold mt-8 max-w-96 ">
                  ScreenExtend is device management
                </h1>
                <p className="text-lg mt-4">
                  All your connected devices in one place.
                </p>
              </div>
            </div>
          </div>
          <div className="relative z-20 flex items-center text-lg font-medium text-primary">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="mr-2 h-6 w-6"
            >
              <path d="M15 6v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3" />
            </svg>
            ScreenExtend
          </div>
        </div>
        <div className="lg:p-8">{children}</div>
      </div>
    </>
  );
};

export default AuthLayout;
