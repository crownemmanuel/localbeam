export interface MeInfo {
  id: string;
  name: string;
  avatar: string;
  host: string | null;
  transfer_port: number;
  http_port: number;
  save_dir: string;
  allow_mode: "all" | "contacts";
  require_accept: boolean;
  enable_qr_server: boolean;
  qr_url: string | null;
}

export interface PeerInfo {
  id: string;
  name: string;
  avatar: string;
  host: string;
  transfer_port: number;
  http_port: number;
  mobile_web_available: boolean;
  last_seen: number;
}

export interface Contact {
  id: string;
  name: string;
  avatar: string;
  added_at: number;
}

export interface ContactRequest {
  id: string;
  name: string;
  avatar: string;
  message: string | null;
  requested_at: number;
}

export interface ContactRequestInbound {
  id: string;
  name: string;
  avatar: string;
  message: string | null;
}

export interface TransferFileMeta {
  name: string;
  size: number;
  mime: string | null;
}

export type TransferDirection = "outgoing" | "incoming";
export type TransferStatus =
  | "pending"
  | "active"
  | "completed"
  | "failed"
  | "rejected"
  | "cancelled";

export interface TransferProgress {
  transfer_id: string;
  created_at: number;
  direction: TransferDirection;
  peer_id: string | null;
  peer_name: string;
  files: TransferFileMeta[];
  current_file_index: number;
  bytes_sent: number;
  total_bytes: number;
  status: TransferStatus;
  error: string | null;
}

export interface IncomingPrompt {
  transfer_id: string;
  from_id: string;
  from_name: string;
  from_avatar: string;
  files: TransferFileMeta[];
  total_bytes: number;
  source: "peer" | "qr_upload";
}
