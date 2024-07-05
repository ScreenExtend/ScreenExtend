import { useState, useContext } from "react";
import { useNavigate } from "react-router-dom";
import { Input } from "../ui/input";
import { Button } from "@/components/ui/button";
import {
  Form,
  FormControl,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "@/components/ui/form";
import { Eye, EyeOff } from "lucide-react";
import { AuthProviderContext } from "@/components/auth-provider";
import { useTheme, Theme } from "@/components/theme-provider";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { zodResolver } from "@hookform/resolvers/zod";

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

  function onSubmit(values: z.infer<typeof formSchema>) {
    setError(false);
    if (values.username.length == 0) {
      document.getElementById("guestLogin")!.click();
    } else {
      if (!Object.keys(localStorage).some(x => x.startsWith(values.username)) || localStorage.getItem(values.username + "-password") === values.password) {
        setCurrentUser({username: values.username, password: values.password});
        localStorage.setItem(values.username + "-username", values.username);
        localStorage.setItem(values.username + "-password", values.password);
        localStorage.setItem(values.username + "-theme", localStorage.getItem(values.username + "-theme") || theme);
        setTheme((localStorage.getItem(values.username + "-theme") || theme) as Theme);
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
                <div className="relative outline-none" style={{ marginBottom: "10px" }}>
                  <Input
                    type={showPassword ? "text" : "password"}
                    placeholder="Password"
                    className="outline-none"
                    autoComplete="off"
                    hoverLabel={false}
                    {...field}
                  />
                  <div className="absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer">
                    {showPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        onClick={() => setShowPassword((prev) => !prev)}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        onClick={() => setShowPassword((prev) => !prev)}
                      />
                    )}
                  </div>
                </div>
              </FormControl>
              <FormMessage style={{ display: (error ? "initial" : "none"), fontWeight: "bolder" }} className="text-red-500">Please enter the correct password.</FormMessage>
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
