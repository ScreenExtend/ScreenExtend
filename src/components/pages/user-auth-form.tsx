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
import { z } from "zod";
import { zodResolver } from "@hookform/resolvers/zod";
import { useForm } from "react-hook-form";
import { Eye, EyeOff } from "lucide-react";
import { useState } from "react";

const formSchema = z.object({
  username: z.string(),
  password: z.string(),
});
export function UserAuthForm() {
  const form = useForm<z.infer<typeof formSchema>>({
    resolver: zodResolver(formSchema),
    defaultValues: {
      username: "",
      password: "",
    },
  });
  const [showPassword, setShowPassword] = useState(false);

  function onSubmit(values: z.infer<typeof formSchema>) {
    console.log(values);
  }

  function togglePasswordVisibility() {
    setShowPassword((prev) => !prev);
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
                <Input placeholder="JohnDoe" {...field} />
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
                <div className="relative outline-none">
                  <Input
                    type={showPassword ? "text" : "password"}
                    placeholder="Password"
                  className="outline-none"
                    {...field}
                  />
                  <div className="absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 cursor-pointer">
                    {showPassword ? (
                      <EyeOff
                        className="h-5 w-5"
                        onClick={togglePasswordVisibility}
                      />
                    ) : (
                      <Eye
                        className="h-5 w-5"
                        onClick={togglePasswordVisibility}
                      />
                    )}
                  </div>
                </div>
              </FormControl>
              <FormMessage />
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
