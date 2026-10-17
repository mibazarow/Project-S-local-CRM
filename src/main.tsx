import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { AppProvider } from "./state/AppStore";
import "./styles/tokens.css";
import "./styles/app.css";

if (navigator.platform.startsWith("Mac")) {
  document.documentElement.dataset.os = "mac";
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <AppProvider>
      <App />
    </AppProvider>
  </StrictMode>,
);
