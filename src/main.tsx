import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { AppProvider } from "./state/AppStore";
import UiScaleApplier from "./components/shared/UiScaleApplier";
import "./styles/tokens.css";
import "./styles/app.css";

if (navigator.platform.startsWith("Mac")) {
  document.documentElement.dataset.os = "mac";
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <AppProvider>
      <UiScaleApplier />
      <App />
    </AppProvider>
  </StrictMode>,
);
