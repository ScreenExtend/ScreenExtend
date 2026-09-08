import "@/lib/legacy-webview-polyfills";

import ReactDOM from "react-dom/client";

import App from "./App.tsx";
import { Toaster } from "@/components/ui/toaster";
import { installErrorReporting } from "@/lib/error-reporting";

import "./index.css";

installErrorReporting();

// index.html's boot-failure panel renders into #root and gives up if it is missing, so a
// bare `!` here would turn a missing root into a blank window with nothing to read.
const root = document.getElementById("root");
if (!root) {
  throw new Error("index.html did not provide #root; the app cannot mount");
}

ReactDOM.createRoot(root).render(
  <>
    <App />
    <Toaster />
  </>
);
