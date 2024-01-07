import { UserAuthForm } from "@/components/pages/user-auth-form";
import { buttonVariants } from "@/components/ui/button";
import AuthLayout from "@/layout/auth-layout";
import { cn } from "@/lib/utils";
import { Link } from "react-router-dom";

export default function Login() {
  return (
    <AuthLayout
      navData={{
        label: "Sign Up",
        href: "/signup",
      }}
    >
      <div className="mx-auto flex w-full flex-col space-y-6 sm:w-[350px] h-screen justify-center md:h-full">
        <div className="flex flex-col space-y-2">
          <h1 className="text-2xl font-semibold tracking-tight text-left">
            Log In
          </h1>
        </div>
        <UserAuthForm />
        <p className="text-sm text-muted-foreground">
          <Link
            to="/forgot-password"
            className="underline underline-offset-4 hover:text-primary"
          >
            Forgot your password?
          </Link>
        </p>
        <div className="relative">
          <div className="absolute inset-0 flex items-center">
            <span className="w-full border-t" />
          </div>
          <div className="relative flex justify-center text-xs uppercase">
            <span className="bg-background px-2 text-muted-foreground">Or</span>
          </div>
        </div>
        <div className="flex justify-center">
          <Link
            to="/dashboard"
            className={cn(
              buttonVariants({ variant: "secondary" }),
              "w-full justify-center"
            )}
          >
            Login as Guest
          </Link>
        </div>
      </div>
    </AuthLayout>
  );
}
