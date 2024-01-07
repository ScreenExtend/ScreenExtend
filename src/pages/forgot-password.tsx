import { PasswordResetAuthForm } from "@/components/pages/reset-password-form";
import AuthLayout from "@/layout/auth-layout";
import { Link } from "react-router-dom";

export default function ForgotPassword() {
  return (
    <AuthLayout
      navData={{
        label: "Log In",
        href: "/",
      }}
    >
      <div className="mx-auto flex w-full flex-col space-y-6 sm:w-[350px] h-screen justify-center md:h-full">
        <div className="flex flex-col space-y-2">
          <h1 className="text-2xl font-semibold tracking-tight text-left">
            Forgot Password
          </h1>
        </div>
        <PasswordResetAuthForm />
        <p className="text-sm text-muted-foreground">
          Remember your password? &nbsp;
          <Link
            to="/"
            className="underline underline-offset-4 hover:text-primary"
          >
            Log In instead
          </Link>
        </p>
      </div>
    </AuthLayout>
  );
}
