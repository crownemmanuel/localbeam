use crate::{
    contacts::now_secs,
    state::{
        AppState, IncomingPrompt, TransferDirection, TransferFileMeta, TransferProgress,
        TransferSource, TransferStatus,
    },
};
use anyhow::Result;
use tauri::Manager;
use axum::{
    Json, Router,
    extract::{Multipart, Path as AxPath, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
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

pub async fn start_server(state: Arc<AppState>) -> Result<()> {
    let port = state.settings.read().http_port;
    let http_state = HttpState {
        app: state.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index))
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

async fn announce(
    State(s): State<HttpState>,
    Json(body): Json<AnnouncePayload>,
) -> Response {
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

    let _ = now_secs(); // silence unused import in some builds
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

    let mut t = match s
        .app
        .active_transfers
        .read()
        .get(&transfer_id)
        .cloned()
    {
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
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("create: {e}"))
                    .into_response();
            }
        };

        while let Ok(Some(chunk)) = field.chunk().await {
            let n = chunk.len();
            if let Err(e) = out.write_all(&chunk).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}"))
                    .into_response();
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
        out.trim_start_matches('.')
            .to_string()
            .replace("..", "_")
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
  :root { color-scheme: light dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: linear-gradient(180deg, #0f172a, #1e293b);
    color: #f8fafc;
    min-height: 100vh;
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 24px 16px 48px;
  }
  .card {
    width: 100%; max-width: 480px;
    background: rgba(255,255,255,0.06);
    border: 1px solid rgba(255,255,255,0.1);
    border-radius: 16px;
    padding: 20px;
    backdrop-filter: blur(8px);
  }
  h1 { font-size: 18px; margin: 0 0 4px; font-weight: 600; }
  .sub { font-size: 13px; opacity: 0.7; margin-bottom: 16px; }
  .device { display: flex; align-items: center; gap: 10px; margin-bottom: 18px; }
  .device .avatar {
    width: 36px; height: 36px; border-radius: 10px;
    background: linear-gradient(135deg, #3b82f6 0%, #5dd4c0 100%);
    display: flex; align-items: center; justify-content: center;
    font-size: 20px;
  }
  .device .name { font-weight: 600; }
  .drop {
    border: 2px dashed rgba(255,255,255,0.25);
    border-radius: 12px;
    padding: 36px 16px;
    text-align: center;
    transition: 0.15s;
    margin-bottom: 14px;
  }
  .drop.over { border-color: #60a5fa; background: rgba(59,130,246,0.10); }
  .drop p { margin: 8px 0 0; opacity: 0.7; font-size: 13px; }
  input[type=file] { display: none; }
  button, label.btn {
    appearance: none; border: 0; cursor: pointer;
    background: #3b82f6; color: white;
    padding: 12px 18px; border-radius: 10px;
    font-weight: 600; font-size: 14px;
    display: inline-block;
  }
  button:disabled { opacity: 0.5; cursor: default; }
  .files { margin-top: 16px; }
  .file {
    display: flex; justify-content: space-between; align-items: center;
    padding: 8px 12px; border-radius: 8px;
    background: rgba(255,255,255,0.04); margin-bottom: 6px;
    font-size: 13px;
  }
  .file .size { opacity: 0.6; font-variant-numeric: tabular-nums; }
  .progress {
    height: 6px; background: rgba(255,255,255,0.08); border-radius: 4px; overflow: hidden;
    margin-top: 10px;
  }
  .progress > div { height: 100%; background: linear-gradient(90deg, #3b82f6, #5dd4c0); width: 0%; transition: width 0.2s; }
  .status { font-size: 13px; margin-top: 10px; min-height: 18px; opacity: 0.85; }
  .actions { display: flex; gap: 10px; margin-top: 16px; }
  .actions button { flex: 1; }
  .secondary { background: rgba(255,255,255,0.1); }
</style>
</head>
<body>
<div class="card">
  <h1>Send to this computer</h1>
  <div class="sub">via LocalBeam · LAN only</div>
  <div class="device">
    <div class="avatar">💻</div>
    <div><div class="name">{{DEVICE_NAME}}</div><div class="sub" style="margin:0">Waiting for files…</div></div>
  </div>

  <label class="drop" id="drop">
    <input id="picker" type="file" multiple />
    <div style="font-size:36px">📤</div>
    <div style="margin-top:8px;font-weight:600">Tap to choose files</div>
    <p>or drag &amp; drop</p>
  </label>

  <div class="files" id="files"></div>
  <div class="actions">
    <button class="secondary" id="clear" disabled>Clear</button>
    <button id="send" disabled>Send</button>
  </div>
  <div class="progress" style="display:none" id="progress"><div></div></div>
  <div class="status" id="status"></div>
</div>

<script>
  const picker = document.getElementById('picker');
  const drop = document.getElementById('drop');
  const filesEl = document.getElementById('files');
  const sendBtn = document.getElementById('send');
  const clearBtn = document.getElementById('clear');
  const status = document.getElementById('status');
  const progress = document.getElementById('progress');
  const progressBar = progress.firstElementChild;
  let files = [];
  let senderName = localStorage.getItem('lb-name') || promptName();

  function promptName() {
    const n = prompt('Your name (shown to the computer)', 'Phone') || 'Phone';
    localStorage.setItem('lb-name', n);
    return n;
  }

  function fmt(n){
    if (n < 1024) return n + ' B';
    if (n < 1024*1024) return (n/1024).toFixed(1) + ' KB';
    if (n < 1024*1024*1024) return (n/1024/1024).toFixed(1) + ' MB';
    return (n/1024/1024/1024).toFixed(2) + ' GB';
  }

  function render() {
    filesEl.innerHTML = '';
    for (const f of files) {
      const div = document.createElement('div');
      div.className = 'file';
      div.innerHTML = '<span>' + escapeHtml(f.name) + '</span><span class="size">' + fmt(f.size) + '</span>';
      filesEl.appendChild(div);
    }
    sendBtn.disabled = files.length === 0;
    clearBtn.disabled = files.length === 0;
  }
  function escapeHtml(s){ return s.replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }

  picker.addEventListener('change', e => {
    files = files.concat(Array.from(e.target.files));
    render();
  });

  ;['dragenter','dragover'].forEach(ev => drop.addEventListener(ev, e => { e.preventDefault(); drop.classList.add('over'); }));
  ;['dragleave','drop'].forEach(ev => drop.addEventListener(ev, e => { e.preventDefault(); drop.classList.remove('over'); }));
  drop.addEventListener('drop', e => {
    if (e.dataTransfer && e.dataTransfer.files) {
      files = files.concat(Array.from(e.dataTransfer.files));
      render();
    }
  });

  clearBtn.addEventListener('click', () => { files = []; render(); status.textContent=''; progress.style.display='none'; });

  sendBtn.addEventListener('click', async () => {
    sendBtn.disabled = true; clearBtn.disabled = true; picker.disabled = true;
    status.textContent = 'Waiting for the computer to accept…';

    let manifest = files.map(f => ({ name: f.name, size: f.size, mime: f.type || null }));
    let res;
    try {
      res = await fetch('/announce', { method: 'POST', headers: {'Content-Type':'application/json'}, body: JSON.stringify({ from_name: senderName, files: manifest }) });
    } catch(e) { status.textContent = 'Network error: ' + e.message; sendBtn.disabled = false; return; }
    if (res.status === 403) { status.textContent = 'Rejected by the computer.'; clearBtn.disabled=false; return; }
    if (!res.ok) { status.textContent = 'Error: ' + res.status; sendBtn.disabled=false; clearBtn.disabled=false; return; }
    const { transfer_id } = await res.json();

    status.textContent = 'Uploading…';
    progress.style.display = 'block';

    const form = new FormData();
    for (const f of files) form.append('file', f, f.name);

    const xhr = new XMLHttpRequest();
    xhr.open('POST', '/upload/' + encodeURIComponent(transfer_id));
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) progressBar.style.width = (100 * e.loaded / e.total).toFixed(1) + '%';
    };
    xhr.onload = () => {
      if (xhr.status === 200) { progressBar.style.width='100%'; status.textContent = 'Done ✓'; files=[]; render(); }
      else status.textContent = 'Upload failed: ' + xhr.status;
      clearBtn.disabled = false; picker.disabled = false;
    };
    xhr.onerror = () => { status.textContent = 'Upload network error'; clearBtn.disabled = false; picker.disabled = false; };
    xhr.send(form);
  });
</script>
</body>
</html>"##;
