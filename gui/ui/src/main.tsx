import { Component, StrictMode } from "react";
import type { ErrorInfo, ReactNode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { reportFailure } from "./ipc";
import "./app.css";

/**
 * Catches a render failure and reports it out of the web view.
 *
 * React's own default for an uncaught error is to log it to the web view's
 * console, which nothing outside this process ever reads — verified by breaking
 * a component on purpose and watching the process log stay empty. Without this
 * boundary a broken component produces a blank window and a clean stderr, which
 * is the one failure shape this front-end must never have.
 */
class ErrorBoundary extends Component<{ children: ReactNode }, { failed: string | null }> {
  override state: { failed: string | null } = { failed: null };

  static getDerivedStateFromError(error: unknown) {
    return { failed: String(error) };
  }

  override componentDidCatch(error: unknown, info: ErrorInfo) {
    reportFailure("render", `${String(error)} ${info.componentStack ?? ""}`);
  }

  override render() {
    if (this.state.failed !== null) {
      return (
        <div className="grid h-full place-items-center p-8 text-center text-sm text-ink-dim">
          Youta&rsquo;s window failed to render: {this.state.failed}
        </div>
      );
    }
    return this.props.children;
  }
}

const container = document.getElementById("root");
if (container === null) {
  reportFailure("mount", "the window has no root element");
} else {
  createRoot(container).render(
    <StrictMode>
      <ErrorBoundary>
        <App />
      </ErrorBoundary>
    </StrictMode>,
  );
}

// A web view swallows its own errors, so anything that escapes React is copied
// out to the process log. Without this a broken window is indistinguishable
// from a working one from the outside.
window.addEventListener("error", (event) => {
  reportFailure("uncaught error", String(event.message));
});
window.addEventListener("unhandledrejection", (event) => {
  reportFailure("unhandled rejection", String(event.reason));
});
