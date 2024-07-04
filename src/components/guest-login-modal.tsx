import {
    AlertDialog,
    AlertDialogAction,
    AlertDialogCancel,
    AlertDialogContent,
    AlertDialogDescription,
    AlertDialogFooter,
    AlertDialogHeader,
    AlertDialogTitle,
    AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { useState } from "react";
import { useNavigate } from "react-router-dom";

export function GuestLoginModal() {
    const navigate = useNavigate();
    const [dontShowAgain, setDontShowAgain] = useState(true);
    
    return (
        <AlertDialog>
            <AlertDialogTrigger asChild>
                <Button variant="outline" size={"sm"} className="w-full justify-center" id={"guestLogin"}>
                    Login as Guest
                </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
                <AlertDialogHeader>
                    <AlertDialogTitle>Continue As Guest?</AlertDialogTitle>
                    <AlertDialogDescription>
                        As a guest, your session won't be saved. Consider using an account
                        to save your preferences.
                    </AlertDialogDescription>
                </AlertDialogHeader>
                <div className="flex items-center space-x-2 mb-4">
                    <Checkbox
                        id="dontShowAgain"
                        checked={dontShowAgain}
                        onCheckedChange={(checked) => setDontShowAgain(checked === true)}
                    />
                    <label
                        htmlFor="dontShowAgain"
                        className="text-sm text-muted-foreground cursor-pointer"
                    >
                        Don't show this message again
                    </label>
                </div>
                <AlertDialogFooter>
                    <AlertDialogCancel>Cancel</AlertDialogCancel>
                    <AlertDialogAction onClick={() => navigate("/dashboard")}>
                        Continue
                    </AlertDialogAction>
                </AlertDialogFooter>
            </AlertDialogContent>
        </AlertDialog>
        );
}