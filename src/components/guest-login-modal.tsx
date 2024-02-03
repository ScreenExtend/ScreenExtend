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
import { useNavigate } from "react-router-dom";

export function GuestLoginModal() {
    const navigate = useNavigate();
    
    return (
        <AlertDialog>
            <AlertDialogTrigger asChild>
                <Button variant="outline" size={"sm"} className="w-full justify-center">
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