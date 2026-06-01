use crate::{
    contacts::now_secs,
    state::{
        AppState, IncomingPrompt, TransferDirection, TransferFileMeta, TransferProgress,
        TransferSource, TransferStatus,
    },
};
use anyhow::Result;
use axum::{
    extract::{Multipart, Path as AxPath, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tauri::Manager;
use tokio::{io::AsyncWriteExt, sync::oneshot, time::timeout};

#[derive(Clone)]
struct HttpState {
    app: Arc<AppState>,
    sessions: Arc<Mutex<HashMap<String, UploadSession>>>,
}

#[derive(Clone)]
#[allow(dead_code)]
struct UploadSession {
    transfer_id: String,
    from_name: String,
    save_dir: PathBuf,
    accepted: bool,
}

#[derive(Serialize, Deserialize)]
struct AnnouncePayload {
    from_name: String,
    files: Vec<TransferFileMeta>,
}

#[derive(Serialize)]
struct AnnounceResponse {
    transfer_id: String,
}

#[derive(Serialize)]
struct MobilePeerSummary {
    id: String,
    name: String,
    host: String,
    http_port: u16,
    mobile_web_available: bool,
    url: String,
}

pub async fn start_server(state: Arc<AppState>) -> Result<()> {
    let port = state.settings.read().http_port;
    let http_state = HttpState {
        app: state.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/peers", get(peers))
        .route("/announce", post(announce))
        .route("/upload/:id", post(upload))
        .route("/health", get(health))
        .with_state(http_state)
        // 8 GiB cap on a single upload session (axum default is smaller).
        .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024 * 1024));

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    tracing::info!("qr http server listening on http://0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn index(State(s): State<HttpState>) -> Html<String> {
    let device_name = s.app.settings.read().device_name.clone();
    Html(INDEX_HTML.replace("{{DEVICE_NAME}}", &html_escape(&device_name)))
}

async fn peers(State(s): State<HttpState>) -> Json<Vec<MobilePeerSummary>> {
    let mut peers: Vec<_> = s
        .app
        .peers
        .read()
        .values()
        .cloned()
        .map(|peer| MobilePeerSummary {
            id: peer.id,
            name: peer.name,
            host: peer.host.clone(),
            http_port: peer.http_port,
            mobile_web_available: peer.mobile_web_available,
            url: format!("http://{}:{}/", peer.host, peer.http_port),
        })
        .collect();
    peers.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.host.cmp(&b.host))
    });
    Json(peers)
}

async fn announce(State(s): State<HttpState>, Json(body): Json<AnnouncePayload>) -> Response {
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let total_bytes: u64 = body.files.iter().map(|f| f.size).sum();
    let prompt = IncomingPrompt {
        transfer_id: transfer_id.clone(),
        from_id: "qr-mobile".into(),
        from_name: body.from_name.clone(),
        from_avatar: "📱".into(),
        files: body.files.clone(),
        total_bytes,
        source: TransferSource::QrUpload,
    };

    s.app.update_transfer(TransferProgress {
        transfer_id: transfer_id.clone(),
        created_at: now_secs(),
        direction: TransferDirection::Incoming,
        peer_id: None,
        peer_name: body.from_name.clone(),
        files: body.files.clone(),
        current_file_index: 0,
        bytes_sent: 0,
        total_bytes,
        status: TransferStatus::Pending,
        error: None,
    });

    let (tx, rx) = oneshot::channel::<bool>();
    s.app
        .pending_decisions
        .write()
        .insert(transfer_id.clone(), tx);
    s.app.emit("incoming-transfer", prompt);
    if let Some(app_handle) = s.app.app.read().clone() {
        if let Some(win) = app_handle.get_webview_window("main") {
            let _ = win.show();
            let _ = win.set_focus();
        }
    }

    let accepted = match timeout(Duration::from_secs(180), rx).await {
        Ok(Ok(v)) => v,
        _ => {
            s.app.pending_decisions.write().remove(&transfer_id);
            false
        }
    };

    if !accepted {
        let mut t = s
            .app
            .active_transfers
            .read()
            .get(&transfer_id)
            .cloned()
            .unwrap();
        t.status = TransferStatus::Rejected;
        s.app.update_transfer(t);
        return (StatusCode::FORBIDDEN, "rejected").into_response();
    }

    let save_dir = PathBuf::from(&s.app.settings.read().save_dir);
    s.sessions.lock().insert(
        transfer_id.clone(),
        UploadSession {
            transfer_id: transfer_id.clone(),
            from_name: body.from_name,
            save_dir,
            accepted: true,
        },
    );

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&AnnounceResponse { transfer_id }).unwrap(),
    )
        .into_response()
}

#[derive(Deserialize)]
struct UploadQuery {
    #[allow(dead_code)]
    name: Option<String>,
}

async fn upload(
    State(s): State<HttpState>,
    AxPath(transfer_id): AxPath<String>,
    Query(_q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Response {
    let session = match s.sessions.lock().get(&transfer_id).cloned() {
        Some(x) if x.accepted => x,
        _ => return (StatusCode::FORBIDDEN, "session not accepted").into_response(),
    };

    let mut t = match s.app.active_transfers.read().get(&transfer_id).cloned() {
        Some(t) => t,
        None => return (StatusCode::NOT_FOUND, "transfer not found").into_response(),
    };
    t.status = TransferStatus::Active;
    s.app.update_transfer(t.clone());

    let mut bytes_recv: u64 = 0;
    let mut file_idx: usize = 0;

    tokio::fs::create_dir_all(&session.save_dir).await.ok();

    while let Some(mut field) = match multipart.next_field().await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("multipart err: {e}")).into_response();
        }
    } {
        let raw_name = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("upload-{}", uuid::Uuid::new_v4()));
        let safe = sanitize_filename(&raw_name);
        let target = unique_path(&session.save_dir, &safe);
        let mut out = match tokio::fs::File::create(&target).await {
            Ok(f) => f,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("create: {e}")).into_response();
            }
        };

        while let Ok(Some(chunk)) = field.chunk().await {
            let n = chunk.len();
            if let Err(e) = out.write_all(&chunk).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")).into_response();
            }
            bytes_recv += n as u64;
            if bytes_recv.is_multiple_of(256 * 1024) || bytes_recv == t.total_bytes {
                let mut tt = s
                    .app
                    .active_transfers
                    .read()
                    .get(&transfer_id)
                    .cloned()
                    .unwrap_or(t.clone());
                tt.current_file_index = file_idx;
                tt.bytes_sent = bytes_recv;
                s.app.update_transfer(tt);
            }
        }
        out.flush().await.ok();
        file_idx += 1;
    }

    let mut tt = s
        .app
        .active_transfers
        .read()
        .get(&transfer_id)
        .cloned()
        .unwrap_or(t.clone());
    tt.status = TransferStatus::Completed;
    tt.bytes_sent = tt.total_bytes.max(bytes_recv);
    s.app.update_transfer(tt);
    s.sessions.lock().remove(&transfer_id);
    let _ = &session.from_name;
    (StatusCode::OK, "ok").into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | '\0' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
    }
    if out.is_empty() {
        "file".into()
    } else {
        out.trim_start_matches('.').to_string().replace("..", "_")
    }
}

fn unique_path(dir: &PathBuf, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{}", e)),
        None => (name.to_string(), String::new()),
    };
    for i in 1..10_000 {
        let p = dir.join(format!("{} ({}){}", stem, i, ext));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{}-{}{}", stem, uuid::Uuid::new_v4(), ext))
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover" />
<title>Send to {{DEVICE_NAME}} · LocalBeam</title>
<style>
  :root {
    color-scheme: dark;
    --bg: #0c0e16;
    --bg2: #12151f;
    --bg3: #181c28;
    --bg4: #1e2234;
    --border: rgba(255,255,255,0.07);
    --border2: rgba(255,255,255,0.12);
    --text: #eceef8;
    --text2: #8b90a8;
    --text3: #555a72;
    --accent: #3b82f6;
    --accent2: #60a5fa;
    --mint2: #5dd4c0;
    --green: #34d399;
    --red: #f87171;
    --radius: 18px;
    --radius-sm: 12px;
    font-family: -apple-system, BlinkMacSystemFont, "Inter", "Segoe UI", Roboto, sans-serif;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
  }

  * { box-sizing: border-box; }

  body {
    margin: 0;
    min-height: 100vh;
    color: var(--text);
    background:
      radial-gradient(circle at top, rgba(59,130,246,0.16), transparent 36%),
      linear-gradient(180deg, #0a0d15 0%, var(--bg) 55%, #090b12 100%);
  }

  .shell {
    width: min(100%, 560px);
    margin: 0 auto;
    padding: 24px 16px 40px;
  }

  .hero {
    display: flex;
    gap: 14px;
    align-items: flex-start;
    margin-bottom: 18px;
  }

  .hero-mark,
  .target-icon,
  .drop-icon,
  .peer-icon {
    flex-shrink: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #fff;
  }

  .hero-mark {
    width: 52px;
    height: 52px;
    border-radius: 16px;
    background: linear-gradient(135deg, var(--accent) 0%, var(--mint2) 100%);
    box-shadow: 0 16px 34px rgba(59,130,246,0.25);
  }

  .eyebrow {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--accent2);
    margin-bottom: 6px;
  }

  h1 {
    margin: 0;
    font-size: 28px;
    line-height: 1.05;
    letter-spacing: -0.03em;
  }

  .hero-copy p {
    margin: 10px 0 0;
    font-size: 14px;
    line-height: 1.45;
    color: var(--text2);
  }

  .panel {
    background: rgba(18,21,31,0.94);
    border: 1px solid var(--border2);
    border-radius: var(--radius);
    box-shadow: 0 24px 60px rgba(0,0,0,0.35);
    overflow: hidden;
  }

  .panel-body {
    padding: 18px;
  }

  .row-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 10px;
  }

  .label {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text3);
  }

  button,
  input,
  label.pick {
    font: inherit;
  }

  button {
    appearance: none;
    border: 0;
    cursor: pointer;
    border-radius: var(--radius-sm);
    padding: 12px 14px;
    font-size: 14px;
    font-weight: 700;
    line-height: 1;
    transition: transform 0.12s ease, opacity 0.12s ease, background 0.12s ease, border-color 0.12s ease;
  }

  button:hover { opacity: 0.92; }
  button:active { transform: scale(0.98); }
  button:disabled { opacity: 0.35; cursor: default; transform: none; }

  .primary {
    background: var(--accent);
    color: #fff;
  }

  .secondary {
    background: var(--bg3);
    color: var(--text2);
    border: 1px solid var(--border);
  }

  .ghost {
    background: transparent;
    color: var(--accent2);
    padding: 0;
  }

  .target-card {
    width: 100%;
    border: 1px solid var(--border);
    border-radius: 16px;
    background: var(--bg2);
    color: inherit;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 14px;
    text-align: left;
    margin-bottom: 16px;
  }

  .target-card .target-copy {
    flex: 1;
    min-width: 0;
  }

  .target-icon {
    width: 42px;
    height: 42px;
    border-radius: 12px;
    background: var(--bg3);
    color: var(--accent2);
  }

  .target-name {
    font-size: 15px;
    font-weight: 700;
    margin-bottom: 3px;
  }

  .target-meta {
    font-size: 12px;
    color: var(--text2);
  }

  .target-hint {
    font-size: 11px;
    color: var(--accent2);
    font-weight: 700;
    white-space: nowrap;
  }

  .field {
    margin-bottom: 16px;
  }

  .field label {
    display: block;
    margin-bottom: 6px;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text3);
  }

  input[type="text"] {
    width: 100%;
    border: 1px solid var(--border2);
    border-radius: var(--radius-sm);
    background: var(--bg3);
    color: var(--text);
    padding: 12px 13px;
    outline: none;
  }

  input[type="text"]:focus {
    border-color: var(--accent);
  }

  input[type="file"] {
    display: none;
  }

  .drop {
    display: block;
    border: 1.5px dashed var(--border2);
    border-radius: 16px;
    padding: 22px 18px;
    text-align: center;
    background:
      linear-gradient(180deg, rgba(59,130,246,0.08), rgba(24,28,40,0.4));
    transition: background 0.12s ease, border-color 0.12s ease;
  }

  .drop.over {
    border-color: var(--accent);
    background:
      linear-gradient(180deg, rgba(59,130,246,0.16), rgba(24,28,40,0.65));
  }

  .drop-icon {
    width: 52px;
    height: 52px;
    margin: 0 auto 12px;
    border-radius: 16px;
    background: rgba(59,130,246,0.12);
    color: var(--accent2);
  }

  .drop-title {
    font-size: 16px;
    font-weight: 700;
    margin-bottom: 4px;
  }

  .drop-sub {
    font-size: 13px;
    line-height: 1.45;
    color: var(--text2);
  }

  .files {
    margin-top: 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .file {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 12px;
    border-radius: 12px;
    background: var(--bg3);
    font-size: 13px;
  }

  .file-name {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .file-size {
    flex-shrink: 0;
    color: var(--text2);
    font-variant-numeric: tabular-nums;
  }

  .actions {
    display: flex;
    gap: 10px;
    margin-top: 16px;
  }

  .actions button {
    flex: 1;
  }

  .progress {
    display: none;
    height: 5px;
    margin-top: 14px;
    border-radius: 999px;
    overflow: hidden;
    background: var(--bg3);
  }

  .progress > div {
    width: 0%;
    height: 100%;
    background: linear-gradient(90deg, var(--accent) 0%, var(--mint2) 100%);
    transition: width 0.18s ease;
  }

  .status {
    min-height: 20px;
    margin-top: 12px;
    font-size: 13px;
    color: var(--text2);
  }

  .status.success { color: var(--green); }
  .status.error { color: var(--red); }

  .footnote {
    margin-top: 14px;
    font-size: 12px;
    line-height: 1.45;
    color: var(--text3);
  }

  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(5,6,12,0.72);
    backdrop-filter: blur(6px);
    display: flex;
    align-items: flex-end;
    justify-content: center;
    padding: 16px;
  }

  .overlay[hidden] {
    display: none;
  }

  .sheet {
    width: min(100%, 560px);
    max-height: min(72vh, 620px);
    background: var(--bg2);
    border: 1px solid var(--border2);
    border-radius: 22px;
    box-shadow: 0 24px 60px rgba(0,0,0,0.45);
    overflow: hidden;
  }

  .sheet-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 18px 18px 10px;
  }

  .sheet-head h2 {
    margin: 0;
    font-size: 18px;
  }

  .sheet-sub {
    padding: 0 18px 14px;
    font-size: 13px;
    line-height: 1.45;
    color: var(--text2);
    border-bottom: 1px solid var(--border);
  }

  .peer-list {
    padding: 10px 12px 14px;
    overflow: auto;
  }

  .peer-row {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 13px 12px;
    border-radius: 14px;
    border: 1px solid transparent;
    background: transparent;
    color: inherit;
    text-align: left;
    margin-top: 4px;
  }

  .peer-row:hover {
    background: var(--bg3);
    border-color: var(--border);
  }

  .peer-row:disabled {
    background: transparent;
    border-color: transparent;
    opacity: 0.55;
  }

  .peer-icon {
    width: 40px;
    height: 40px;
    border-radius: 12px;
    background: var(--bg3);
    color: var(--accent2);
  }

  .peer-copy {
    flex: 1;
    min-width: 0;
  }

  .peer-name {
    font-size: 14px;
    font-weight: 700;
    margin-bottom: 3px;
  }

  .peer-meta {
    font-size: 12px;
    color: var(--text2);
  }

  .peer-pill {
    flex-shrink: 0;
    font-size: 11px;
    font-weight: 700;
    padding: 6px 8px;
    border-radius: 999px;
    background: rgba(52,211,153,0.12);
    color: var(--green);
  }

  .peer-pill.off {
    background: var(--bg3);
    color: var(--text3);
  }

  .empty {
    padding: 28px 14px;
    text-align: center;
    color: var(--text2);
    font-size: 13px;
    line-height: 1.5;
  }

  svg {
    width: 24px;
    height: 24px;
    stroke: currentColor;
    fill: none;
    stroke-width: 1.7;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  @media (max-width: 420px) {
    .shell {
      padding-inline: 12px;
    }

    h1 {
      font-size: 24px;
    }

    .panel-body,
    .sheet-head,
    .sheet-sub {
      padding-left: 16px;
      padding-right: 16px;
    }
  }
</style>
</head>
<body>
  <div class="shell">
    <div class="hero">
      <div class="hero-mark" aria-hidden="true"></div>
      <div class="hero-copy">
        <div class="eyebrow">LocalBeam mobile upload</div>
        <h1>Send to this computer</h1>
        <p>Choose the computer you want, then upload files directly over your local network.</p>
      </div>
    </div>

    <div class="panel">
      <div class="panel-body">
        <div class="row-head">
          <div class="label">Current target</div>
          <button type="button" class="ghost" id="browsePeers">See more computers</button>
        </div>

        <button type="button" class="target-card" id="targetCard">
          <div class="target-icon" id="targetIcon" aria-hidden="true"></div>
          <div class="target-copy">
            <div class="target-name" id="targetName">{{DEVICE_NAME}}</div>
            <div class="target-meta" id="targetMeta">This LocalBeam host is ready for files.</div>
          </div>
          <div class="target-hint">Switch</div>
        </button>

        <div class="field">
          <label for="senderName">Your name</label>
          <input id="senderName" type="text" maxlength="48" placeholder="Phone" autocapitalize="words" autocomplete="name" />
        </div>

        <label class="drop" id="drop">
          <input id="picker" type="file" multiple />
          <div class="drop-icon" id="dropIcon" aria-hidden="true"></div>
          <div class="drop-title">Tap to choose files</div>
          <div class="drop-sub">You can also drag and drop from another app.</div>
        </label>

        <div class="files" id="files"></div>
        <div class="actions">
          <button type="button" class="secondary" id="clear" disabled>Clear</button>
          <button type="button" class="primary" id="send" disabled>Send</button>
        </div>
        <div class="progress" id="progress"><div></div></div>
        <div class="status" id="status"></div>
        <div class="footnote">Use “See more computers” to switch to another LocalBeam host this machine has already discovered.</div>
      </div>
    </div>
  </div>

  <div class="overlay" id="peerOverlay" hidden>
    <div class="sheet" role="dialog" aria-modal="true" aria-labelledby="peerTitle">
      <div class="sheet-head">
        <h2 id="peerTitle">Nearby computers</h2>
        <button type="button" class="ghost" id="closePeers">Close</button>
      </div>
      <div class="sheet-sub">These are the other LocalBeam computers that <strong>{{DEVICE_NAME}}</strong> can currently see on the network.</div>
      <div class="peer-list" id="peerList">
        <div class="empty">Loading nearby computers…</div>
      </div>
    </div>
  </div>

<script>
  const DEVICE_NAME = '{{DEVICE_NAME}}';
  const picker = document.getElementById('picker');
  const drop = document.getElementById('drop');
  const filesEl = document.getElementById('files');
  const sendBtn = document.getElementById('send');
  const clearBtn = document.getElementById('clear');
  const status = document.getElementById('status');
  const progress = document.getElementById('progress');
  const progressBar = progress.firstElementChild;
  const senderInput = document.getElementById('senderName');
  const browsePeersBtn = document.getElementById('browsePeers');
  const targetCard = document.getElementById('targetCard');
  const targetName = document.getElementById('targetName');
  const targetMeta = document.getElementById('targetMeta');
  const targetIcon = document.getElementById('targetIcon');
  const dropIcon = document.getElementById('dropIcon');
  const peerOverlay = document.getElementById('peerOverlay');
  const closePeersBtn = document.getElementById('closePeers');
  const peerList = document.getElementById('peerList');
  const query = new URLSearchParams(window.location.search);
  const carriedSender = query.get('sender');
  let files = [];

  if (carriedSender) {
    localStorage.setItem('lb-name', carriedSender);
    query.delete('sender');
    const cleanQuery = query.toString();
    const cleanUrl = window.location.pathname + (cleanQuery ? '?' + cleanQuery : '') + window.location.hash;
    history.replaceState({}, '', cleanUrl);
  }

  senderInput.value = (carriedSender || localStorage.getItem('lb-name') || 'Phone').slice(0, 48);
  targetName.textContent = DEVICE_NAME;
  targetMeta.textContent = window.location.host;
  targetIcon.innerHTML = deviceIconSvg('computer');
  dropIcon.innerHTML = uploadIconSvg();
  document.querySelector('.hero-mark').innerHTML = deviceIconSvg('computer');

  function fmt(n) {
    if (n < 1024) return n + ' B';
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
    if (n < 1024 * 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + ' MB';
    return (n / 1024 / 1024 / 1024).toFixed(2) + ' GB';
  }

  function sanitizeSenderName() {
    const trimmed = senderInput.value.trim().slice(0, 48) || 'Phone';
    senderInput.value = trimmed;
    localStorage.setItem('lb-name', trimmed);
    return trimmed;
  }

  function setStatus(text, tone) {
    status.textContent = text || '';
    status.className = 'status' + (tone ? ' ' + tone : '');
  }

  function render() {
    filesEl.innerHTML = '';
    for (const f of files) {
      const row = document.createElement('div');
      row.className = 'file';

      const name = document.createElement('div');
      name.className = 'file-name';
      name.textContent = f.name;

      const size = document.createElement('div');
      size.className = 'file-size';
      size.textContent = fmt(f.size);

      row.appendChild(name);
      row.appendChild(size);
      filesEl.appendChild(row);
    }

    sendBtn.disabled = files.length === 0;
    clearBtn.disabled = files.length === 0;
  }

  function deviceKind(name) {
    const lower = String(name || '').toLowerCase();
    if (/phone|android|iphone|mobile/.test(lower)) return 'phone';
    if (/desktop|pc|windows|linux|studio/.test(lower)) return 'monitor';
    return 'computer';
  }

  function deviceIconSvg(kind) {
    if (kind === 'phone') {
      return '<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="5" y="2" width="14" height="20" rx="3"></rect><path d="M12 18h.01" stroke-width="2.5"></path></svg>';
    }
    if (kind === 'monitor') {
      return '<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="2" y="3" width="20" height="14" rx="2"></rect><path d="M8 21h8"></path><path d="M12 17v4"></path></svg>';
    }
    return '<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="13" rx="2"></rect><path d="M1 21h22"></path><path d="M7 21l1.5-4h7L17 21"></path></svg>';
  }

  function uploadIconSvg() {
    return '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="17 8 12 3 7 8"></polyline><line x1="12" y1="3" x2="12" y2="15"></line></svg>';
  }

  async function openPeerPicker() {
    peerOverlay.hidden = false;
    peerList.innerHTML = '<div class="empty">Loading nearby computers…</div>';
    try {
      const res = await fetch('/peers', { cache: 'no-store' });
      if (!res.ok) throw new Error('Request failed with ' + res.status);
      const peers = await res.json();
      renderPeerList(peers);
    } catch (err) {
      peerList.innerHTML = '';
      const empty = document.createElement('div');
      empty.className = 'empty';
      empty.textContent = 'Could not load nearby computers. ' + (err && err.message ? err.message : '');
      peerList.appendChild(empty);
    }
  }

  function closePeerPicker() {
    peerOverlay.hidden = true;
  }

  function renderPeerList(peers) {
    peerList.innerHTML = '';
    if (!Array.isArray(peers) || peers.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'empty';
      empty.textContent = 'No other computers found yet. Open LocalBeam on another computer on the same Wi-Fi.';
      peerList.appendChild(empty);
      return;
    }

    for (const peer of peers) {
      const row = document.createElement('button');
      row.type = 'button';
      row.className = 'peer-row';
      row.disabled = !peer.mobile_web_available;

      const icon = document.createElement('div');
      icon.className = 'peer-icon';
      icon.innerHTML = deviceIconSvg(deviceKind(peer.name));

      const copy = document.createElement('div');
      copy.className = 'peer-copy';
      const name = document.createElement('div');
      name.className = 'peer-name';
      name.textContent = peer.name;
      const meta = document.createElement('div');
      meta.className = 'peer-meta';
      meta.textContent = peer.host + ':' + peer.http_port;
      copy.appendChild(name);
      copy.appendChild(meta);

      const pill = document.createElement('div');
      pill.className = 'peer-pill' + (peer.mobile_web_available ? '' : ' off');
      pill.textContent = peer.mobile_web_available ? 'Available' : 'Mobile off';

      row.appendChild(icon);
      row.appendChild(copy);
      row.appendChild(pill);
      row.addEventListener('click', () => switchToPeer(peer));
      peerList.appendChild(row);
    }
  }

  function switchToPeer(peer) {
    if (!peer.mobile_web_available) {
      setStatus(peer.name + ' does not have mobile uploads enabled.', 'error');
      closePeerPicker();
      return;
    }
    if (files.length > 0 && !window.confirm('Switch computers? Your selected files will need to be picked again.')) {
      return;
    }
    const sender = encodeURIComponent(sanitizeSenderName());
    window.location.href = peer.url + '?sender=' + sender;
  }

  senderInput.addEventListener('change', sanitizeSenderName);
  senderInput.addEventListener('blur', sanitizeSenderName);

  picker.addEventListener('change', (e) => {
    const picked = Array.from(e.target.files || []);
    if (picked.length > 0) {
      files = files.concat(picked);
      render();
    }
  });

  ['dragenter', 'dragover'].forEach((eventName) => {
    drop.addEventListener(eventName, (e) => {
      e.preventDefault();
      drop.classList.add('over');
    });
  });

  ['dragleave', 'drop'].forEach((eventName) => {
    drop.addEventListener(eventName, (e) => {
      e.preventDefault();
      drop.classList.remove('over');
    });
  });

  drop.addEventListener('drop', (e) => {
    const dropped = Array.from((e.dataTransfer && e.dataTransfer.files) || []);
    if (dropped.length > 0) {
      files = files.concat(dropped);
      render();
    }
  });

  clearBtn.addEventListener('click', () => {
    files = [];
    picker.value = '';
    progress.style.display = 'none';
    progressBar.style.width = '0%';
    setStatus('');
    render();
  });

  browsePeersBtn.addEventListener('click', openPeerPicker);
  targetCard.addEventListener('click', openPeerPicker);
  closePeersBtn.addEventListener('click', closePeerPicker);
  peerOverlay.addEventListener('click', (e) => {
    if (e.target === peerOverlay) closePeerPicker();
  });

  sendBtn.addEventListener('click', async () => {
    const senderName = sanitizeSenderName();
    sendBtn.disabled = true;
    clearBtn.disabled = true;
    picker.disabled = true;
    setStatus('Waiting for ' + DEVICE_NAME + ' to accept…');

    const manifest = files.map((f) => ({ name: f.name, size: f.size, mime: f.type || null }));
    let res;

    try {
      res = await fetch('/announce', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ from_name: senderName, files: manifest }),
      });
    } catch (err) {
      setStatus('Network error: ' + err.message, 'error');
      sendBtn.disabled = false;
      clearBtn.disabled = false;
      picker.disabled = false;
      return;
    }

    if (res.status === 403) {
      setStatus('Rejected by the computer.', 'error');
      clearBtn.disabled = false;
      picker.disabled = false;
      return;
    }

    if (!res.ok) {
      setStatus('Upload could not start. Error ' + res.status + '.', 'error');
      sendBtn.disabled = false;
      clearBtn.disabled = false;
      picker.disabled = false;
      return;
    }

    const payload = await res.json();
    progress.style.display = 'block';
    setStatus('Uploading…');

    const form = new FormData();
    for (const f of files) form.append('file', f, f.name);

    const xhr = new XMLHttpRequest();
    xhr.open('POST', '/upload/' + encodeURIComponent(payload.transfer_id));
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) {
        progressBar.style.width = (100 * e.loaded / e.total).toFixed(1) + '%';
      }
    };
    xhr.onload = () => {
      if (xhr.status === 200) {
        progressBar.style.width = '100%';
        files = [];
        picker.value = '';
        render();
        setStatus('Done.', 'success');
      } else {
        setStatus('Upload failed: ' + xhr.status, 'error');
      }
      clearBtn.disabled = false;
      picker.disabled = false;
    };
    xhr.onerror = () => {
      setStatus('Upload network error.', 'error');
      clearBtn.disabled = false;
      picker.disabled = false;
    };
    xhr.send(form);
  });

  render();
</script>
</body>
</html>"##;
