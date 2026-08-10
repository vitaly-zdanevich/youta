// Tauri is loaded from the window rather than a package, because the window
// sets `withGlobalTauri` and the CSP forbids anything else being fetched.
declare global {
  interface Window {
    __TAURI__?: {
      core: { invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> };
      event: {
        listen<T>(
          event: string,
          handler: (event: { payload: T }) => void,
        ): Promise<() => void>;
      };
    };
  }
}

export {};
