import { RegisterAuthForm } from "@/components/pages/register-auth-form";
import AuthLayout from "@/layout/auth-layout";
import { Link } from "react-router-dom";

export default function SignUp() {
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
            Create an Account
          </h1>
        </div>
        <RegisterAuthForm />
        <p className="px-8 text-center text-sm text-muted-foreground">
          By clicking continue, you agree to our{" "}
          <Link
            to="/terms"
            className="underline underline-offset-4 hover:text-primary"
          >
            Terms of Service
          </Link>
          .
        </p>
      </div>
    </AuthLayout>
  );
}
