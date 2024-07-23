import { useState, useContext } from "react";
import { useNavigate } from "react-router-dom";

import { Input } from "../ui/input";
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

import { AuthProviderContext, getUser, createUser } from "@/components/auth-provider";
import { useTheme, type Theme } from "@/components/theme-provider";
import { useForm } from "react-hook-form";
import { setup } from "@/lib/bindings";
import { zodResolver } from "@hookform/resolvers/zod";
import { z } from "zod";
import { cn } from "@/lib/utils";

const formSchema = z.object({
  username: z.string(),
  password: z.string(),
});
export function UserAuthForm() {
  const { setCurrentUser } = useContext(AuthProviderContext);
  const { theme, setTheme } = useTheme();
  const navigate = useNavigate();

  const [error, setError] = useState(false);

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
    if (values.username.length == 0) {
      document.getElementById("guestLogin")!.click();
    } else {
      const user = getUser(values.username);
      if (!user) {
        createUser({username: values.username, password: values.password, theme});
        setCurrentUser(values.username);
        await setup();
        navigate("/dashboard");
      } else if (user.password === values.password) {
        setCurrentUser(values.username);
        setTheme(user.theme as Theme);
        await setup();
        navigate("/dashboard");
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
    </Form>
  );
}
