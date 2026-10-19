// App-wide async confirmation channel. `window.confirm` is broken in
// WKWebView/Tauri: it returns before the user answers, so destructive actions
// ran before the dialog resolved. Instead every destructive action awaits
// `confirmAction`/`confirmRemove`, and one mounted <ConfirmDialogHost/> outlet
// (see components/shared/ConfirmDialogHost.tsx) renders the dialog and
// resolves the promise. Without a mounted host the promise resolves false.

export interface ConfirmOptions {
  title: string;
  message?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
}

export interface ConfirmRequest {
  id: number;
  opts: ConfirmOptions;
  resolve: (ok: boolean) => void;
}

type HostHandler = (req: ConfirmRequest) => void;

let host: HostHandler | null = null;
let pending: ConfirmRequest | null = null;
let nextId = 1;

export function registerConfirmHost(handler: HostHandler | null): void {
  if (handler === null && pending) {
    const req = pending;
    pending = null;
    req.resolve(false);
  }
  host = handler;
}

export function confirmAction(opts: ConfirmOptions): Promise<boolean> {
  const h = host;
  if (!h) return Promise.resolve(false);
  if (pending) {
    pending.resolve(false);
    pending = null;
  }
  return new Promise<boolean>((resolve) => {
    const req: ConfirmRequest = {
      id: nextId++,
      opts,
      resolve: (ok) => {
        if (pending === req) pending = null;
        resolve(ok);
      },
    };
    pending = req;
    h(req);
  });
}

export function confirmRemove(opts: ConfirmOptions): Promise<boolean> {
  return confirmAction({ confirmLabel: "Удалить", danger: true, ...opts });
}
