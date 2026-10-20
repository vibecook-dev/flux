/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Unset or "mock": in-browser mock backend. "/": same-origin flux-server. Or an absolute URL. */
  readonly VITE_FLUX_API?: string;
}
