import "@/lib/legacy-webview-polyfills";

import ReactDOM from "react-dom/client";

import App from "./App.tsx";
import { Toaster } from "@/components/ui/toaster";
import { installErrorReporting } from "@/lib/error-reporting";

import "./index.css";

installErrorReporting();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <>
    <App />
    <Toaster />
  </>
);
