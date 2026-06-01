import { invoke } from "@tauri-apps/api/core";
import type {
  Contact,
  ContactRequest,
  MeInfo,
  PeerInfo,
  TransferProgress,
} from "./types";

export const api = {
  getMe: () => invoke<MeInfo>("get_me"),
  listPeers: () => invoke<PeerInfo[]>("list_peers"),
  listContacts: () => invoke<Contact[]>("list_contacts"),
  listPendingContactRequests: () =>
    invoke<ContactRequest[]>("list_pending_contact_requests"),
  listTransfers: () => invoke<TransferProgress[]>("list_transfers"),
  updateSettings: (patch: Record<string, unknown>) =>
    invoke<unknown>("update_settings", { patch }),
  addManualPeer: (
    host: string,
    name: string | null,
    transferPort: number | null,
    httpPort: number | null
  ) =>
    invoke<PeerInfo>("add_manual_peer", {
      host,
      name,
      transferPort,
      httpPort,
    }),
  removeManualPeer: (peerId: string) =>
    invoke<void>("remove_manual_peer", { peerId }),
  sendFiles: (peerId: string, paths: string[]) =>
    invoke<string>("send_files", { peerId, paths }),
  sendContactRequest: (peerId: string, message: string | null) =>
    invoke<void>("send_contact_request", { peerId, message }),
  decideIncoming: (transferId: string, accept: boolean) =>
    invoke<void>("decide_incoming", { transferId, accept }),
  acceptContactRequest: (id: string) =>
    invoke<Contact | null>("accept_contact_request", { id }),
  rejectContactRequest: (id: string) =>
    invoke<void>("reject_contact_request", { id }),
  removeContact: (id: string) => invoke<void>("remove_contact", { id }),
  clearCompleted: () => invoke<void>("clear_completed_transfers"),
  republishDiscovery: () => invoke<void>("republish_discovery"),
  hideWindow: () => invoke<void>("hide_window"),
  quit: () => invoke<void>("quit_app"),
  setAutoHide: (enabled: boolean) => invoke<void>("set_auto_hide", { enabled }),
};

/** Run an async function with the auto-hide-on-blur guard suspended.
 *  Use around native dialogs so opening them doesn't dismiss the main window. */
export async function withDialogFocus<T>(fn: () => Promise<T>): Promise<T> {
  await api.setAutoHide(false).catch(() => {});
  try {
    return await fn();
  } finally {
    await api.setAutoHide(true).catch(() => {});
  }
}
