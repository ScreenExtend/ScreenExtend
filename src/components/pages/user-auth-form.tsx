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

import { AuthProviderContext, getUser, createUser, deleteUser } from "@/components/auth-provider";
import { GlobalProviderContext } from "@/components/global-provider";
import { useTheme, type Theme } from "@/components/theme-provider";
import { zodResolver } from "@hookform/resolvers/zod";
import { cn, generateSlug } from "@/lib/utils";
import { commands } from "@/lib/bindings";
import { useForm } from "react-hook-form";
import { z } from "zod";

const formSchema = z.object({
  username: z.string(),
  password: z.string(),
});
export function UserAuthForm() {
  const { setCurrentUser } = useContext(AuthProviderContext);
  const { windowAuthValues: [authValues, setAuthValues], windowLoaded: [loaded, setLoaded], windowOtp: [, setOtp], windowHostedNetworkOn: [, setHostedNetworkOn], windowSlug: [, setSlug], windowQrValues: [, setQrValues] } = useContext(GlobalProviderContext);
  const { theme, setTheme } = useTheme();
  const navigate = useNavigate();

  const [usernameError, setUsernameError] = useState(false);
  const [passwordError, setPasswordError] = useState(false);
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
    setAuthValues(values);
    setUsernameError(false);
    setPasswordError(false);
    if (values.username.length === 0) {
      setUsernameError(true);
    } else {
      const user = await getUser(values.username);
      if (user && user.password !== values.password) {
        setPasswordError(true);
      } else {
        let success;
        if (!loaded) {
          success = await commands.setup();
          setLoaded(success);
        } else {
          success = loaded;
        }
        if (success) {
          setOtp([...Array(6)].reduce(a=>a+"0123456789"[~~(Math.random()*"0123456789".length)], ""));
          setHostedNetworkOn(false);
          setSlug(generateSlug());
          await commands.removeAllDisplays();
          await deleteUser("GUESTGUESTGUESTGUESTGUEST");
          setQrValues([{ title: "Local Hosted Network", value: "https://screenextend.app/ascsa" }, { title: "Same As Current Device", value: "https://screenextend.app/adb" }, { title: "Any Wifi Network", value: "https://screenextend.app/" }]);
          if (!user) {
            await createUser({username: values.username, password: values.password, theme});
          } else {
            await setTheme(user.theme as Theme);
          }
          setCurrentUser(values.username);
          await commands.setCurrentUser(values.username);
          navigate("/dashboard");
        } else {
          setSetupError(true);
        }
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
              <FormLabel>Username <span style={{ color: "red" }}>*</span></FormLabel>
              <FormControl>
                <div className="relative outline-none" style={{ marginBottom: "20px" }}>
                  <Input
                    type="text"
                    placeholder="Username"
                    className={cn("outline-none", usernameError && "border-red-500 focus:ring-red-500")}
                    autoComplete="off"
                    hoverLabel={false}
                    maxLength={19}
                    {...field}
                  />
                  <p style={{ display: (usernameError ? "initial" : "none"), position: "absolute", marginTop: "3px" }} className="text-red-500 text-xs">Invalid username (try logging in as guest instead)</p>
                </div>
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
                    className={cn("outline-none", passwordError && "border-red-500 focus:ring-red-500")}
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
                  <p style={{ display: (passwordError ? "initial" : "none"), position: "absolute", marginTop: "3px" }} className="text-red-500 text-xs">Incorrect password</p>
                </div>
              </FormControl>
            </FormItem>
          )}
        />
        {/* <div className="flex items-center space-x-2 mb-4">
          <Checkbox
            id="rememberMe"
            checked={rememberMe}
            onCheckedChange={checked => setRememberMe(checked === true)}
          />
          <label
            htmlFor="rememberMe"
            className="text-sm text-muted-foreground cursor-pointer"
          >
            Keep me logged in
          </label>
        </div> */}
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
            <AlertDialogCancel disabled={loading} onClick={() => setSetupError(false)} className="disabled:cursor-not-allowed disabled:select-none disabled:opacity-50">Go Back</AlertDialogCancel>
            <AlertDialogAction
              className="bg-blue-600 hover:bg-blue-700 text-white disabled:cursor-not-allowed disabled:select-none disabled:opacity-50"
              onClick={async () => {
                setLoading(true);
                await commands.installDrivers();
                await new Promise(resolve => setTimeout(resolve, 5000));
                setLoading(false);
                setSetupError(false);
                await onSubmit(authValues);
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
