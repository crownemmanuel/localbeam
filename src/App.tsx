import { useCallback, useEffect, useMemo, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openPath, openUrl } from "@tauri-apps/plugin-opener";
import { relaunch } from "@tauri-apps/plugin-process";
import { check } from "@tauri-apps/plugin-updater";
import QRCode from "qrcode";
import { api, withDialogFocus } from "./api";
import type {
  Contact,
  ContactRequest,
  IncomingPrompt,
  MeInfo,
  PeerInfo,
  TransferProgress,
} from "./types";
import {
  ArrowDownIcon,
  ArrowLeftIcon,
  ArrowUpIcon,
  CheckIcon,
  DownloadIcon,
  FolderOpenIcon,
  LaptopIcon,
  MonitorIcon,
  PhoneIcon,
  QrCodeIcon,
  RefreshIcon,
  SettingsIcon,
  TrashIcon,
  UploadIcon,
  UserIcon,
  UserPlusIcon,
  XIcon,
  SignalIcon,
} from "./icons";

type Tab = "devices" | "transfers" | "contacts" | "settings";

export default function App() {
  const [me, setMe] = useState<MeInfo | null>(null);
  const [peers, setPeers] = useState<PeerInfo[]>([]);
  const [transfers, setTransfers] = useState<TransferProgress[]>([]);
  const [contacts, setContacts] = useState<Contact[]>([]);
  const [pendingContacts, setPendingContacts] = useState<ContactRequest[]>([]);
  const [tab, setTab] = useState<Tab>("devices");
  const [selectedPeer, setSelectedPeer] = useState<PeerInfo | null>(null);
  const [pendingPrompt, setPendingPrompt] = useState<IncomingPrompt | null>(null);
  const [qrOpen, setQrOpen] = useState(false);
  const [dragOver, setDragOver] = useState(false);
  const [draggedPaths, setDraggedPaths] = useState<string[] | null>(null);

  const reloadAll = useCallback(async () => {
    try {
      const [m, ps, ts, cs, pcs] = await Promise.all([
        api.getMe(),
        api.listPeers(),
        api.listTransfers(),
        api.listContacts(),
        api.listPendingContactRequests(),
      ]);
      setMe(m);
      setPeers(ps);
      setTransfers(ts);
      setContacts(cs);
      setPendingContacts(pcs);
    } catch (e) {
      console.error("reload", e);
    }
  }, []);

  useEffect(() => { reloadAll(); }, [reloadAll]);

  useEffect(() => {
    const offs: UnlistenFn[] = [];
    (async () => {
      offs.push(await listen<PeerInfo[]>("peers", (e) => setPeers(e.payload)));
      offs.push(await listen<TransferProgress>("transfer-progress", (e) => {
        setTransfers((cur) => {
          const idx = cur.findIndex((t) => t.transfer_id === e.payload.transfer_id);
          if (idx === -1) return [...cur, e.payload];
          const next = cur.slice();
          next[idx] = e.payload;
          return next;
        });
      }));
      offs.push(await listen<IncomingPrompt>("incoming-transfer", (e) => {
        setPendingPrompt(e.payload);
      }));
      offs.push(await listen("contact-request", () => {
        api.listPendingContactRequests().then(setPendingContacts);
      }));
      const wv = getCurrentWebview();
      const dragOff = await wv.onDragDropEvent((ev) => {
        if (ev.payload.type === "enter" || ev.payload.type === "over") {
          setDragOver(true);
        } else if (ev.payload.type === "leave") {
          setDragOver(false);
          setDraggedPaths(null);
        } else if (ev.payload.type === "drop") {
          setDragOver(false);
          setDraggedPaths((ev.payload as { paths: string[] }).paths);
        }
      });
      offs.push(dragOff);
    })();
    return () => { offs.forEach((o) => o()); };
  }, []);

  useEffect(() => {
    if (draggedPaths && draggedPaths.length > 0) {
      setTab("devices");
      if (selectedPeer) {
        sendToPeer(selectedPeer.id, draggedPaths).finally(() => setDraggedPaths(null));
      }
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draggedPaths, selectedPeer]);

  const sendToPeer = useCallback(async (peerId: string, paths: string[]) => {
    try { await api.sendFiles(peerId, paths); } catch (e) { console.error("send", e); }
  }, []);

  const onChooseFiles = useCallback(async () => {
    if (!selectedPeer) return;
    const result = await withDialogFocus(() => openDialog({ multiple: true }));
    if (!result) return;
    const paths = Array.isArray(result) ? result : [result];
    await sendToPeer(selectedPeer.id, paths.map(String));
  }, [selectedPeer, sendToPeer]);

  const incomingPending = useMemo(
    () => transfers.filter((t) => t.direction === "incoming" && t.status === "pending").length,
    [transfers]
  );
  const activeCount = useMemo(
    () => transfers.filter((t) => t.status === "active" || t.status === "pending").length,
    [transfers]
  );

  return (
    <>
      <Header me={me} onQr={() => setQrOpen(true)} onHide={() => api.hideWindow()} onSettings={() => setTab("settings")} />
      <Tabs
        tab={tab} setTab={(t) => { setTab(t); if (t !== "devices") setSelectedPeer(null); }}
        peerCount={peers.length}
        transferBadge={activeCount}
        contactBadge={pendingContacts.length + incomingPending}
      />
      <div className="content">
        {tab === "devices" && (
          <DevicesPanel
            peers={peers} me={me}
            selected={selectedPeer} setSelected={setSelectedPeer}
            onChooseFiles={onChooseFiles}
            dragOver={dragOver}
            onRescan={() => api.republishDiscovery().then(reloadAll)}
            contacts={contacts}
            onRequestContact={(id) => api.sendContactRequest(id, null)}
          />
        )}
        {tab === "transfers" && (
          <TransfersPanel transfers={transfers} saveDir={me?.save_dir ?? ""} onClear={() => api.clearCompleted().then(reloadAll)} />
        )}
        {tab === "contacts" && (
          <ContactsPanel
            contacts={contacts} pending={pendingContacts}
            onAccept={async (id) => { await api.acceptContactRequest(id); reloadAll(); }}
            onReject={async (id) => { await api.rejectContactRequest(id); reloadAll(); }}
            onRemove={async (id) => { await api.removeContact(id); reloadAll(); }}
          />
        )}
        {tab === "settings" && me && (
          <SettingsPanel me={me} onChanged={reloadAll} />
        )}
      </div>

      {pendingPrompt && (
        <IncomingModal
          prompt={pendingPrompt}
          onAccept={async () => { await api.decideIncoming(pendingPrompt.transfer_id, true); setPendingPrompt(null); }}
          onReject={async () => { await api.decideIncoming(pendingPrompt.transfer_id, false); setPendingPrompt(null); }}
        />
      )}
      {qrOpen && me && <QrModal me={me} onClose={() => setQrOpen(false)} />}
    </>
  );
}

// ─── Header ──────────────────────────────────────────────────────────────────

function Header({ me, onQr, onHide, onSettings }: { me: MeInfo | null; onQr: () => void; onHide: () => void; onSettings: () => void }) {
  return (
    <div className="header" data-tauri-drag-region>
      <div className="header-avatar" data-tauri-drag-region>
        <LaptopIcon size={17} />
      </div>
      <div data-tauri-drag-region style={{ flex: 1, minWidth: 0 }}>
        <div className="header-name" data-tauri-drag-region>{me?.name ?? "LocalBeam"}</div>
        <div className="header-sub" data-tauri-drag-region>
          {me?.host ?? "connecting…"} · {me ? (me.allow_mode === "all" ? "Open" : "Contacts only") : ""}
        </div>
      </div>
      <div className="header-actions">
        <button className="ghost" onClick={onQr} title="Mobile QR"><PhoneIcon size={16} /></button>
        <button className="ghost" onClick={onSettings} title="Settings"><SettingsIcon size={16} /></button>
        <button className="ghost" onClick={onHide} title="Hide"><XIcon size={16} /></button>
      </div>
    </div>
  );
}

// ─── Tabs ─────────────────────────────────────────────────────────────────────

function Tabs({ tab, setTab, peerCount, transferBadge, contactBadge }: {
  tab: Tab; setTab: (t: Tab) => void;
  peerCount: number; transferBadge: number; contactBadge: number;
}) {
  const items: { id: Tab; icon: React.ReactNode; label: string; badge?: number }[] = [
    { id: "devices",   icon: <SignalIcon size={14} />,   label: "Devices",   badge: peerCount || undefined },
    { id: "transfers", icon: <ArrowUpIcon size={14} />,  label: "Transfers", badge: transferBadge || undefined },
    { id: "contacts",  icon: <UserIcon size={14} />,     label: "Contacts",  badge: contactBadge || undefined },
    { id: "settings",  icon: <SettingsIcon size={14} />, label: "Settings" },
  ];
  return (
    <div className="tabs">
      {items.map((it) => (
        <div key={it.id} className={`tab ${tab === it.id ? "active" : ""}`} onClick={() => setTab(it.id)}>
          {it.icon}
          {it.label}
          {it.badge ? <span className="badge">{it.badge}</span> : null}
        </div>
      ))}
    </div>
  );
}

// ─── Device panel ─────────────────────────────────────────────────────────────

function DeviceIcon({ name }: { name: string }) {
  const lower = name.toLowerCase();
  if (/phone|android|iphone|mobile/.test(lower)) return <PhoneIcon size={22} />;
  if (/desktop|pc|windows|linux/.test(lower)) return <MonitorIcon size={22} />;
  return <LaptopIcon size={22} />;
}

function DevicesPanel({ peers, me, selected, setSelected, onChooseFiles, dragOver, onRescan, contacts, onRequestContact }: {
  peers: PeerInfo[]; me: MeInfo | null;
  selected: PeerInfo | null; setSelected: (p: PeerInfo | null) => void;
  onChooseFiles: () => void; dragOver: boolean;
  onRescan: () => void; contacts: Contact[];
  onRequestContact: (id: string) => void;
}) {
  if (selected) {
    const isContact = contacts.some((c) => c.id === selected.id);
    return (
      <div className="send-panel">
        <div style={{ marginBottom: 10 }}>
          <button className="ghost" style={{ padding: "6px 8px", marginLeft: -8 }} onClick={() => setSelected(null)}>
            <ArrowLeftIcon size={15} /> Back
          </button>
        </div>
        <div className="send-peer-row">
          <div className="send-peer-icon"><DeviceIcon name={selected.name} /></div>
          <div>
            <div style={{ fontWeight: 700 }}>{selected.name}</div>
            <div style={{ fontSize: 11, color: "var(--text2)" }}>
              {selected.host}{isContact ? " · contact" : ""}
            </div>
          </div>
        </div>
        <div className={`dropzone ${dragOver ? "over" : ""}`}>
          <div className="dropzone-icon"><UploadIcon size={22} /></div>
          <div className="dropzone-title">Drop files here</div>
          <div className="dropzone-sub">or click to browse</div>
          <button onClick={onChooseFiles} style={{ justifyContent: "center" }}>
            <UploadIcon size={14} /> Choose files
          </button>
        </div>
        {me?.allow_mode === "contacts" && !isContact && (
          <button className="muted full" style={{ marginTop: 4 }} onClick={() => onRequestContact(selected.id)}>
            <UserPlusIcon size={14} /> Send contact request
          </button>
        )}
      </div>
    );
  }

  return (
    <>
      {/* Radar area */}
      <div className="radar-wrap">
        <div className="radar-rings">
          <div className="radar-ring" />
          <div className="radar-ring" />
          <div className="radar-ring" />
          <div className="radar-me">
            <LaptopIcon size={26} />
          </div>
        </div>
        <div className="radar-status">
          {peers.length === 0
            ? "Scanning network…"
            : `${peers.length} device${peers.length === 1 ? "" : "s"} nearby`}
        </div>
      </div>

      {/* Device grid */}
      <div className="device-grid">
        {peers.length === 0 ? (
          <div className="empty" style={{ width: "100%" }}>
            <div>No devices found yet.</div>
            <div>Open LocalBeam on another computer<br />on the same Wi-Fi.</div>
            <button className="muted" style={{ marginTop: 12 }} onClick={onRescan}>
              <RefreshIcon size={13} /> Rescan
            </button>
          </div>
        ) : (
          peers.map((p) => (
            <div key={p.id} className="device-card" onClick={() => setSelected(p)}>
              <div className="device-card-icon"><DeviceIcon name={p.name} /></div>
              <div className="device-card-name">{p.name}</div>
            </div>
          ))
        )}
      </div>
    </>
  );
}

// ─── Transfers panel ──────────────────────────────────────────────────────────

function fmtBytes(n: number) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}

function fmtTransferDate(ts: number) {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(ts * 1000));
}

function TransfersPanel({ transfers, saveDir, onClear }: { transfers: TransferProgress[]; saveDir: string; onClear: () => void }) {
  const sorted = [...transfers].sort((a, b) => b.created_at - a.created_at);
  return (
    <div className="transfer-list">
      <div className="row-between" style={{ marginBottom: 4 }}>
        <span className="label">Transfers</span>
        <button className="ghost" style={{ fontSize: 11 }} onClick={onClear}>
          <TrashIcon size={13} /> Clear done
        </button>
      </div>
      {sorted.length === 0 && (
        <div className="empty">
          <ArrowUpIcon size={28} />
          <div>No transfers yet.</div>
          <div>Pick a device and send some files.</div>
        </div>
      )}
      {sorted.map((t) => {
        const pct = t.total_bytes > 0 ? Math.min(100, (100 * t.bytes_sent) / t.total_bytes) : 0;
        const chipCls =
          t.status === "completed" ? "chip-done" :
          t.status === "failed" || t.status === "rejected" ? "chip-fail" :
          t.status === "active" ? "chip-active" : "chip-pending";
        const chipLabel =
          t.status === "active" ? `${fmtBytes(t.bytes_sent)} / ${fmtBytes(t.total_bytes)}` :
          t.status === "completed" ? "Done" :
          t.status === "failed" ? "Failed" :
          t.status === "rejected" ? "Declined" :
          t.status === "pending" ? "Waiting…" : t.status;
        const dateLabel = t.direction === "incoming" ? "Received" : "Sent";
        return (
          <div key={t.transfer_id} className="transfer-card">
            <div className="transfer-head">
              <div className={`transfer-dir-icon ${t.direction === "outgoing" ? "dir-out" : "dir-in"}`}>
                {t.direction === "outgoing" ? <ArrowUpIcon size={15} /> : <ArrowDownIcon size={15} />}
              </div>
              <div>
                <div className="transfer-peer">{t.peer_name}</div>
                <div className="transfer-meta">
                  {t.files.length} file{t.files.length === 1 ? "" : "s"} · {fmtBytes(t.total_bytes)}
                </div>
                <div className="transfer-meta">
                  {dateLabel} {fmtTransferDate(t.created_at)}
                </div>
              </div>
              <span className={`transfer-status-chip ${chipCls}`}>{chipLabel}</span>
            </div>
            {(t.status === "active" || t.status === "completed") && (
              <div className="progress-bar"><div style={{ width: `${pct}%` }} /></div>
            )}
            {t.status === "completed" && t.direction === "incoming" && (
              <div className="transfer-footer">
                <span className="open-folder" onClick={() => openPath(saveDir).catch(console.error)}>
                  <FolderOpenIcon size={13} /> Open folder
                </span>
              </div>
            )}
            {t.error && <div style={{ fontSize: 11, color: "var(--red)", marginTop: 4 }}>{t.error}</div>}
          </div>
        );
      })}
    </div>
  );
}

// ─── Contacts panel ───────────────────────────────────────────────────────────

function ContactsPanel({ contacts, pending, onAccept, onReject, onRemove }: {
  contacts: Contact[]; pending: ContactRequest[];
  onAccept: (id: string) => Promise<void>;
  onReject: (id: string) => Promise<void>;
  onRemove: (id: string) => Promise<void>;
}) {
  return (
    <div className="contact-list">
      {pending.length > 0 && (
        <>
          <div className="label" style={{ marginBottom: 8 }}>Requests</div>
          {pending.map((p) => (
            <div key={p.id} className="req-card">
              <div className="req-head">
                <div className="contact-icon"><UserPlusIcon size={16} /></div>
                <div>
                  <div className="contact-name">{p.name}</div>
                  <div style={{ fontSize: 11, color: "var(--text2)" }}>{p.message ?? "wants to connect"}</div>
                </div>
              </div>
              <div className="req-actions">
                <button className="muted" onClick={() => onReject(p.id)}><XIcon size={13} /> Reject</button>
                <button onClick={() => onAccept(p.id)}><CheckIcon size={13} /> Accept</button>
              </div>
            </div>
          ))}
          <div className="divider" />
        </>
      )}
      <div className="label" style={{ marginBottom: 8 }}>Contacts</div>
      {contacts.length === 0 && (
        <div className="empty">
          <UserIcon size={28} />
          <div>No contacts yet.</div>
          <div>Send a contact request from a peer's device page.</div>
        </div>
      )}
      {contacts.map((c) => (
        <div key={c.id} className="contact-card">
          <div className="contact-icon"><UserIcon size={16} /></div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div className="contact-name">{c.name}</div>
            <div className="contact-sub">{c.id.slice(0, 14)}…</div>
          </div>
          <button className="ghost" onClick={() => onRemove(c.id)} title="Remove contact">
            <TrashIcon size={15} style={{ color: "var(--red)" }} />
          </button>
        </div>
      ))}
    </div>
  );
}

// ─── Settings panel ───────────────────────────────────────────────────────────

function SettingsPanel({ me, onChanged }: { me: MeInfo; onChanged: () => void }) {
  const [name, setName] = useState(me.name);
  const [saveDir, setSaveDir] = useState(me.save_dir);
  const [allowMode, setAllowMode] = useState<"all" | "contacts">(me.allow_mode);
  const [requireAccept, setRequireAccept] = useState(me.require_accept);
  const [enableQr, setEnableQr] = useState(me.enable_qr_server);
  const [dirty, setDirty] = useState(false);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [updateStatus, setUpdateStatus] = useState("");

  const save = async () => {
    await api.updateSettings({ device_name: name, save_dir: saveDir, allow_mode: allowMode, require_accept: requireAccept, enable_qr_server: enableQr });
    await api.republishDiscovery().catch(() => {});
    setDirty(false);
    onChanged();
  };

  const pickFolder = async () => {
    const sel = await withDialogFocus(() => openDialog({ directory: true }));
    if (typeof sel === "string") { setSaveDir(sel); setDirty(true); }
  };

  const checkForUpdates = async () => {
    setUpdateBusy(true);
    setUpdateStatus("Checking for updates…");
    let update: Awaited<ReturnType<typeof check>> = null;
    try {
      update = await check();
      if (!update) {
        setUpdateStatus("You're up to date.");
        return;
      }

      const details = [
        `Version ${update.version} is available.`,
        update.body?.trim() || "",
        "Download and install it now?",
      ].filter(Boolean).join("\n\n");

      if (!window.confirm(details)) {
        setUpdateStatus(`Update ${update.version} is available.`);
        return;
      }

      let downloaded = 0;
      let contentLength = 0;

      await update.downloadAndInstall((event) => {
        switch (event.event) {
          case "Started":
            contentLength = event.data.contentLength ?? 0;
            setUpdateStatus("Downloading update…");
            break;
          case "Progress":
            downloaded += event.data.chunkLength;
            setUpdateStatus(
              contentLength > 0
                ? `Downloading update… ${Math.min(100, Math.round((downloaded / contentLength) * 100))}%`
                : `Downloading update… ${fmtBytes(downloaded)}`
            );
            break;
          case "Finished":
            setUpdateStatus("Installing update…");
            break;
        }
      });

      setUpdateStatus("Update installed. Restarting…");
      await relaunch();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setUpdateStatus(`Update failed: ${message}`);
    } finally {
      await update?.close().catch(() => {});
      setUpdateBusy(false);
    }
  };

  return (
    <div className="settings-body">
      <div className="field-group">
        <div className="label" style={{ marginBottom: 10 }}>Identity</div>
        <div className="field">
          <label>Device name</label>
          <input value={name} onChange={(e) => { setName(e.target.value); setDirty(true); }} />
        </div>
        <div className="label" style={{ marginBottom: 4 }}>Fingerprint</div>
        <div className="fingerprint">{me.id}</div>
      </div>

      <div className="field-group">
        <div className="label" style={{ marginBottom: 10 }}>Sharing</div>
        <div className="field">
          <label>Accept files from</label>
          <select value={allowMode} onChange={(e) => { setAllowMode(e.target.value as "all" | "contacts"); setDirty(true); }}>
            <option value="all">Anyone on this network</option>
            <option value="contacts">Contacts only</option>
          </select>
        </div>
        <div className="toggle-row" onClick={() => { setRequireAccept(!requireAccept); setDirty(true); }} style={{ cursor: "pointer" }}>
          <div>
            <div className="toggle-label">Ask before each transfer</div>
            <div className="toggle-sub">Show a prompt for every incoming file</div>
          </div>
          <input type="checkbox" checked={requireAccept} onChange={() => {}} style={{ width: "auto", cursor: "pointer" }} />
        </div>
        <div className="toggle-row" onClick={() => { setEnableQr(!enableQr); setDirty(true); }} style={{ cursor: "pointer" }}>
          <div>
            <div className="toggle-label">Mobile QR server</div>
            <div className="toggle-sub">Let phones upload via browser QR</div>
          </div>
          <input type="checkbox" checked={enableQr} onChange={() => {}} style={{ width: "auto", cursor: "pointer" }} />
        </div>
      </div>

      <div className="field-group">
        <div className="label" style={{ marginBottom: 10 }}>Storage</div>
        <div className="field">
          <label>Save received files to</label>
          <div className="field-row">
            <input value={saveDir} onChange={(e) => { setSaveDir(e.target.value); setDirty(true); }} />
            <button className="muted" style={{ flexShrink: 0 }} onClick={pickFolder}>Browse</button>
          </div>
        </div>
        <button className="muted full" onClick={() => openPath(me.save_dir).catch(console.error)}>
          <FolderOpenIcon size={14} /> Open folder
        </button>
      </div>

      <div className="field-group">
        <div className="label" style={{ marginBottom: 10 }}>Updates</div>
        <button
          className="muted full"
          disabled={updateBusy}
          onClick={checkForUpdates}
        >
          <DownloadIcon size={14} /> {updateBusy ? "Updating…" : "Check for updates"}
        </button>
        <div style={{ fontSize: 11, color: "var(--text2)", marginTop: 8, lineHeight: 1.45 }}>
          Updates are pulled from the latest GitHub release.
        </div>
        {updateStatus && (
          <div style={{ fontSize: 11, color: "var(--text2)", marginTop: 8, lineHeight: 1.45 }}>
            {updateStatus}
          </div>
        )}
      </div>

      <div style={{ display: "flex", gap: 8, marginTop: 4 }}>
        <button disabled={!dirty} onClick={save} style={{ flex: 1, justifyContent: "center" }}>
          <CheckIcon size={14} /> Save changes
        </button>
        <button className="danger" onClick={() => api.quit()}>Quit</button>
      </div>
    </div>
  );
}

// ─── Incoming modal ───────────────────────────────────────────────────────────

function IncomingModal({ prompt, onAccept, onReject }: { prompt: IncomingPrompt; onAccept: () => void; onReject: () => void }) {
  return (
    <div className="backdrop">
      <div className="modal">
        <h3>Incoming {prompt.source === "qr_upload" ? "mobile upload" : "transfer"}</h3>
        <div className="row-gap" style={{ marginBottom: 10 }}>
          <div style={{ width: 34, height: 34, borderRadius: 9, background: "var(--bg3)", display: "flex", alignItems: "center", justifyContent: "center" }}>
            {prompt.source === "qr_upload" ? <PhoneIcon size={18} /> : <LaptopIcon size={18} />}
          </div>
          <div>
            <div style={{ fontWeight: 700 }}>{prompt.from_name}</div>
            <div style={{ fontSize: 11, color: "var(--text2)" }}>
              {prompt.files.length} file{prompt.files.length === 1 ? "" : "s"} · {fmtBytes(prompt.total_bytes)}
            </div>
          </div>
        </div>
        <div className="file-list">
          {prompt.files.map((f, i) => (
            <div key={i} className="file-row">
              <span className="name">{f.name}</span>
              <span className="size">{fmtBytes(f.size)}</span>
            </div>
          ))}
        </div>
        <div className="modal-actions">
          <button className="muted" onClick={onReject}><XIcon size={14} /> Decline</button>
          <button onClick={onAccept}><DownloadIcon size={14} /> Accept</button>
        </div>
      </div>
    </div>
  );
}

// ─── QR modal ─────────────────────────────────────────────────────────────────

function QrModal({ me, onClose }: { me: MeInfo; onClose: () => void }) {
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  useEffect(() => {
    if (!me.qr_url) return;
    QRCode.toDataURL(me.qr_url, { width: 300, margin: 1, color: { dark: "#000", light: "#fff" } })
      .then(setDataUrl).catch(console.error);
  }, [me.qr_url]);

  return (
    <div className="backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>Send from your phone</h3>
        {!me.enable_qr_server ? (
          <div className="empty">QR server is disabled in Settings.</div>
        ) : me.qr_url ? (
          <>
            {dataUrl ? <img className="qr-img" src={dataUrl} /> : <div className="empty">Generating…</div>}
            <div className="qr-url" onClick={() => me.qr_url && openUrl(me.qr_url).catch(console.error)}>
              {me.qr_url}
            </div>
            <div style={{ fontSize: 11, color: "var(--text3)", marginTop: 8, textAlign: "center" }}>
              Scan on any phone on the same Wi-Fi
            </div>
          </>
        ) : (
          <div className="empty">No IP address found.</div>
        )}
        <div className="modal-actions">
          <button className="muted full" onClick={onClose}>Close</button>
        </div>
      </div>
    </div>
  );
}
