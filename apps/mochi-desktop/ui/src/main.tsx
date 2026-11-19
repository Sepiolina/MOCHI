import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { QuickWindow } from "./quick/QuickWindow";
import { suppressNativeMenu } from "./list/ContextMenu";
import "./settings/motionChoice";
import "./styles.css";

// The shell's quick verbs open a small second window on the same page, told
// apart by the fragment Rust gives its URL (src-tauri/src/lib.rs).
const compact = window.location.hash === "#quick";

// No browser menu (Refresh, Print, Save as, ...) anywhere but in text fields.
suppressNativeMenu();

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(<StrictMode>{compact ? <QuickWindow /> : <App />}</StrictMode>);
}
