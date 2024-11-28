import { useState, useContext } from "react";
import { useNavigate } from "react-router-dom";

import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Eye, EyeOff } from "lucide-react";
import {
  Form,
  FormControl,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "@/components/ui/form";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle
} from "@/components/ui/alert-dialog";

import { AuthProviderContext, getUser, createUser } from "@/components/auth-provider";
import { useTheme, type Theme } from "@/components/theme-provider";
import { useForm } from "react-hook-form";
import { setup, installDrivers } from "@/lib/bindings";
import { zodResolver } from "@hookform/resolvers/zod";
import { z } from "zod";
import { cn, generateSlug } from "@/lib/utils";

const formSchema = z.object({
  username: z.string(),
  password: z.string(),
});
export function UserAuthForm() {
  const { setCurrentUser } = useContext(AuthProviderContext);
  const { theme, setTheme } = useTheme();
  const navigate = useNavigate();

  const [error, setError] = useState(false);
  const [setupError, setSetupError] = useState(false);
  const [loading, setLoading] = useState(false);

  const form = useForm<z.infer<typeof formSchema>>({
    resolver: zodResolver(formSchema),
    defaultValues: {
      username: "",
      password: "",
    },
  });
  const [showPassword, setShowPassword] = useState(false);

  const onSubmit = async (values: z.infer<typeof formSchema>) => {
    setError(false);
    if (values.username.length === 0) {
      document.getElementById("guestLogin")!.click();
    } else {
      const user = getUser(values.username);
      if (!user) {
        createUser({username: values.username, password: values.password, theme});
        setCurrentUser(values.username);
        const success = await setup();
        if (success) {
          window.slug = generateSlug();
          delete window.qrValues;
          navigate("/dashboard");
        } else {
          setSetupError(true);
        }
      } else if (user.password === values.password) {
        setCurrentUser(values.username);
        setTheme(user.theme as Theme);
        const success = await setup();
        if (success) {
          window.slug = generateSlug();
          delete window.qrValues;
          navigate("/dashboard");
        } else {
          setSetupError(true);
        }
      } else {
        setError(true);
      }
    }
  }

  return (
    <Form {...form}>
      <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-3">
        <FormField
          control={form.control}
          name="username"
          render={({ field }) => (
            <FormItem className="text-left space-y-0">
              <FormLabel>Username</FormLabel>
              <FormControl>
                <Input
                  type="text"
                  placeholder="Username"
                  className="outline-none"
                  autoComplete="off"
                  hoverLabel={false}
                  {...field}
                />
              </FormControl>
              <FormMessage />
            </FormItem>
          )}
        />
        <FormField
          control={form.control}
          name="password"
          render={({ field }) => (
            <FormItem className="space-y-0 text-left">
              <FormLabel>Password</FormLabel>
              <FormControl>
                <div className="relative outline-none" style={{ marginBottom: "20px" }}>
                  <Input
                    type={showPassword ? "text" : "password"}
                    placeholder="Password"
                    className={cn(
                          "outline-none",
                      error && "border-red-500 focus:ring-red-500"
                    )}
                    autoComplete="off"
                    hoverLabel={false}
                    {...field}
                  />
                  <div className="absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer">
                    {showPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        onClick={() => setShowPassword(prev => !prev)}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        onClick={() => setShowPassword(prev => !prev)}
                      />
                    )}
                  </div>
                  <p style={{ display: (error ? "initial" : "none"), position: "absolute", marginTop: "3px" }} className="text-red-500 text-xs">Incorrect password</p>
                </div>
              </FormControl>
            </FormItem>
          )}
        />
        <Button className="w-full" type="submit">
          Submit
        </Button>
      </form>
      <AlertDialog open={setupError}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Driver Setup Error</AlertDialogTitle>
            <AlertDialogDescription>
              There was an error while attepting to intialize drivers. This often occurs due to the drivers not being installed. <b>Click the button below to install the necessary drivers and certificates.</b> If this error is recurring, contact support at <a href="mailto:support@screenextend.app" target="_blank" style={{ textDecoration: "underline" }}>support@screenextend.app</a>.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={loading} onClick={() => setSetupError(false)}>Go Back</AlertDialogCancel>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white"
              onClick={async () => {
                setLoading(true);
                await installDrivers();
                setLoading(false);
                setSetupError(false);
              }}
              disabled={loading}
            >
              Install Drivers
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Form>
  );
}
