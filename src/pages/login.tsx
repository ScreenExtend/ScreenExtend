import { GuestLoginModal } from "@/components/guest-login-modal";
import { UserAuthForm } from "@/components/pages/user-auth-form";
import AuthLayout from "@/layout/auth-layout";

export default function Login() {
  return (
    <AuthLayout>
      <div className="mx-auto flex w-full flex-col space-y-6 sm:w-[350px] h-screen justify-center md:h-full">
        <div className="flex flex-col space-y-2">
          <h1 className="text-2xl font-semibold tracking-tight text-left">
            Login or Sign Up
          </h1>
        </div>
        <UserAuthForm />
        <div className="relative">
          <div className="absolute inset-0 flex items-center">
            <span className="w-full border-t" />
          </div>
          <div className="relative flex justify-center text-xs uppercase">
            <span className="bg-background px-2 text-muted-foreground">Or</span>
          </div>
        </div>
        <div className="justify-center">
          <GuestLoginModal />
        </div>
      </div>
    </AuthLayout>
    );
}
