use crate::{
    contacts::{ContactRequest, now_secs},
    settings::AllowMode,
    state::{
        AppState, IncomingPrompt, TransferDirection, TransferFileMeta, TransferProgress,
        TransferSource, TransferStatus,
    },
};
use anyhow::{Context, Result, anyhow};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
};
use serde::{Deserialize, Serialize};
use std::{
    io::Cursor,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const CHUNK_SIZE: usize = 64 * 1024;
const MAX_HEADER_BYTES: u32 = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeaderMsg {
    Send {
        from: PeerIdent,
        transfer_id: String,
        files: Vec<TransferFileMeta>,
    },
    ContactRequest {
        from: PeerIdent,
        message: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PeerIdent {
    pub id: String,
    pub name: String,
    pub avatar: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponseMsg {
    pub accepted: bool,
    pub reason: Option<String>,
}

// -------- Public API --------

pub async fn start_server(state: Arc<AppState>) -> Result<()> {
    let port = state.settings.read().transfer_port;
    let acceptor = TlsAcceptor::from(Arc::new(server_tls_config(&state)?));
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("bind 0.0.0.0:{}", port))?;
    tracing::info!("transfer server listening on :{}", port);

    loop {
        let (sock, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error=?e, "accept err");
                continue;
            }
        };
        let acc = acceptor.clone();
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(acc, sock, addr, st).await {
                tracing::warn!(error=?e, "incoming conn failed");
            }
        });
    }
}

pub async fn send_files(
    state: Arc<AppState>,
    peer_id: String,
    paths: Vec<PathBuf>,
) -> Result<String> {
    let peer = state
        .peers
        .read()
        .get(&peer_id)
        .cloned()
        .context("peer not found on the network")?;

    let mut files_meta = Vec::with_capacity(paths.len());
    for p in &paths {
        let md = tokio::fs::metadata(p).await.with_context(|| format!("stat {:?}", p))?;
        if !md.is_file() {
            return Err(anyhow!("{:?} is not a regular file", p));
        }
        files_meta.push(TransferFileMeta {
            name: p.file_name().map(|n| n.to_string_lossy().into()).unwrap_or_else(|| "file".into()),
            size: md.len(),
            mime: mime_guess::from_path(p).first().map(|m| m.essence_str().to_string()),
        });
    }
    let total_bytes: u64 = files_meta.iter().map(|f| f.size).sum();
    let transfer_id = uuid::Uuid::new_v4().to_string();

    let (sender_name, sender_avatar) = {
        let s = state.settings.read();
        (s.device_name.clone(), s.device_avatar.clone())
    };

    state.update_transfer(TransferProgress {
        transfer_id: transfer_id.clone(),
        direction: TransferDirection::Outgoing,
        peer_id: Some(peer.id.clone()),
        peer_name: peer.name.clone(),
        files: files_meta.clone(),
        current_file_index: 0,
        bytes_sent: 0,
        total_bytes,
        status: TransferStatus::Pending,
        error: None,
    });

    let result = send_files_inner(
        &state,
        &peer.host,
        peer.transfer_port,
        &transfer_id,
        &peer,
        &paths,
        &files_meta,
        total_bytes,
        &sender_name,
        &sender_avatar,
    )
    .await;

    match result {
        Ok(()) => {
            let mut t = state
                .active_transfers
                .read()
                .get(&transfer_id)
                .cloned()
                .unwrap();
            t.status = TransferStatus::Completed;
            t.bytes_sent = t.total_bytes;
            state.update_transfer(t);
            Ok(transfer_id)
        }
        Err(e) => {
            let mut t = state
                .active_transfers
                .read()
                .get(&transfer_id)
                .cloned()
                .unwrap();
            t.status = TransferStatus::Failed;
            t.error = Some(e.to_string());
            state.update_transfer(t);
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_files_inner(
    state: &Arc<AppState>,
    host: &str,
    port: u16,
    transfer_id: &str,
    peer: &crate::state::PeerInfo,
    paths: &[PathBuf],
    files_meta: &[TransferFileMeta],
    total_bytes: u64,
    sender_name: &str,
    sender_avatar: &str,
) -> Result<()> {
    let connector = TlsConnector::from(Arc::new(client_tls_config()?));
    let stream = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connect {}:{}", host, port))?;
    let dns = ServerName::try_from("localbeam.local").unwrap().to_owned();
    let mut tls = connector.connect(dns, stream).await.context("tls handshake")?;

    let header = HeaderMsg::Send {
        from: PeerIdent {
            id: state.identity.fingerprint.clone(),
            name: sender_name.to_string(),
            avatar: sender_avatar.to_string(),
        },
        transfer_id: transfer_id.to_string(),
        files: files_meta.to_vec(),
    };
    write_msg(&mut tls, &serde_json::to_vec(&header)?).await?;

    let resp_raw = read_msg(&mut tls).await?;
    let resp: ResponseMsg = serde_json::from_slice(&resp_raw)?;
    if !resp.accepted {
        let mut t = state.active_transfers.read().get(transfer_id).cloned().unwrap();
        t.status = TransferStatus::Rejected;
        t.error = resp.reason.clone();
        state.update_transfer(t);
        return Err(anyhow!(resp.reason.unwrap_or_else(|| "rejected".into())));
    }

    // Mark active
    {
        let mut t = state.active_transfers.read().get(transfer_id).cloned().unwrap();
        t.status = TransferStatus::Active;
        state.update_transfer(t);
    }

    let mut bytes_sent: u64 = 0;
    let mut buf = vec![0u8; CHUNK_SIZE];
    for (idx, path) in paths.iter().enumerate() {
        let mut f = tokio::fs::File::open(path).await?;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            tls.write_all(&buf[..n]).await?;
            bytes_sent += n as u64;

            // Update progress (throttled to avoid event spam — every ~256 KiB)
            if bytes_sent % (256 * 1024) < CHUNK_SIZE as u64 || bytes_sent == total_bytes {
                let mut t = state.active_transfers.read().get(transfer_id).cloned().unwrap();
                t.current_file_index = idx;
                t.bytes_sent = bytes_sent;
                t.peer_name = peer.name.clone();
                state.update_transfer(t);
            }
        }
    }
    tls.flush().await?;
    tls.shutdown().await.ok();
    Ok(())
}

async fn handle_conn(
    acc: TlsAcceptor,
    sock: TcpStream,
    addr: SocketAddr,
    state: Arc<AppState>,
) -> Result<()> {
    let mut tls = acc.accept(sock).await.context("tls accept")?;
    let raw = read_msg(&mut tls).await?;
    let header: HeaderMsg = serde_json::from_slice(&raw)?;

    match header {
        HeaderMsg::ContactRequest { from, message } => {
            handle_contact_request(&state, from, message, &mut tls).await
        }
        HeaderMsg::Send {
            from,
            transfer_id,
            files,
        } => handle_incoming_send(&state, addr, from, transfer_id, files, &mut tls).await,
    }
}

async fn handle_contact_request<S>(
    state: &Arc<AppState>,
    from: PeerIdent,
    message: Option<String>,
    tls: &mut S,
) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let blocked = state.contacts.read().is_blocked(&from.id);
    if blocked {
        write_msg(
            tls,
            &serde_json::to_vec(&ResponseMsg {
                accepted: false,
                reason: Some("blocked".into()),
            })?,
        )
        .await?;
        return Ok(());
    }

    {
        let mut book = state.contacts.write();
        book.upsert_pending(ContactRequest {
            id: from.id.clone(),
            name: from.name.clone(),
            avatar: from.avatar.clone(),
            message: message.clone(),
            requested_at: now_secs(),
        });
        book.save(&state.data_dir).ok();
    }
    state.emit(
        "contact-request",
        crate::state::ContactRequestInbound {
            id: from.id,
            name: from.name,
            avatar: from.avatar,
            message,
        },
    );
    write_msg(
        tls,
        &serde_json::to_vec(&ResponseMsg {
            accepted: true,
            reason: None,
        })?,
    )
    .await?;
    Ok(())
}

async fn handle_incoming_send<S>(
    state: &Arc<AppState>,
    _addr: SocketAddr,
    from: PeerIdent,
    transfer_id: String,
    files: Vec<TransferFileMeta>,
    tls: &mut S,
) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // 1) Block / contact-only gating
    let (allow_mode, require_accept, save_dir) = {
        let s = state.settings.read();
        (s.allow_mode.clone(), s.require_accept, PathBuf::from(&s.save_dir))
    };
    let (is_blocked, is_contact) = {
        let book = state.contacts.read();
        (book.is_blocked(&from.id), book.is_contact(&from.id))
    };
    if is_blocked {
        let _ = write_msg(tls, &serde_json::to_vec(&ResponseMsg { accepted: false, reason: Some("blocked".into()) })?).await;
        return Ok(());
    }
    if matches!(allow_mode, AllowMode::Contacts) && !is_contact {
        // Auto-create a pending contact request to surface to the user.
        {
            let mut book = state.contacts.write();
            book.upsert_pending(ContactRequest {
                id: from.id.clone(),
                name: from.name.clone(),
                avatar: from.avatar.clone(),
                message: None,
                requested_at: now_secs(),
            });
            book.save(&state.data_dir).ok();
        }
        state.emit(
            "contact-request",
            crate::state::ContactRequestInbound {
                id: from.id.clone(),
                name: from.name.clone(),
                avatar: from.avatar.clone(),
                message: None,
            },
        );
        let _ = write_msg(
            tls,
            &serde_json::to_vec(&ResponseMsg {
                accepted: false,
                reason: Some("Not in contacts. A contact request was created.".into()),
            })?,
        )
        .await;
        return Ok(());
    }

    // 2) Prompt the user
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let prompt = IncomingPrompt {
        transfer_id: transfer_id.clone(),
        from_id: from.id.clone(),
        from_name: from.name.clone(),
        from_avatar: from.avatar.clone(),
        files: files.clone(),
        total_bytes,
        source: TransferSource::Peer,
    };

    // Insert pending transfer status
    state.update_transfer(TransferProgress {
        transfer_id: transfer_id.clone(),
        direction: TransferDirection::Incoming,
        peer_id: Some(from.id.clone()),
        peer_name: from.name.clone(),
        files: files.clone(),
        current_file_index: 0,
        bytes_sent: 0,
        total_bytes,
        status: TransferStatus::Pending,
        error: None,
    });

    let accepted = if require_accept {
        let (tx, rx) = oneshot::channel();
        state
            .pending_decisions
            .write()
            .insert(transfer_id.clone(), tx);
        state.emit("incoming-transfer", prompt);
        match timeout(Duration::from_secs(120), rx).await {
            Ok(Ok(v)) => v,
            _ => {
                state.pending_decisions.write().remove(&transfer_id);
                false
            }
        }
    } else {
        true
    };

    if !accepted {
        write_msg(
            tls,
            &serde_json::to_vec(&ResponseMsg {
                accepted: false,
                reason: Some("declined".into()),
            })?,
        )
        .await?;
        let mut t = state.active_transfers.read().get(&transfer_id).cloned().unwrap();
        t.status = TransferStatus::Rejected;
        state.update_transfer(t);
        return Ok(());
    }
    write_msg(
        tls,
        &serde_json::to_vec(&ResponseMsg { accepted: true, reason: None })?,
    )
    .await?;

    // 3) Receive files
    tokio::fs::create_dir_all(&save_dir).await.ok();
    let mut bytes_recv: u64 = 0;
    let mut buf = vec![0u8; CHUNK_SIZE];

    for (idx, fmeta) in files.iter().enumerate() {
        let safe_name = sanitize_filename(&fmeta.name);
        let target = unique_path(&save_dir, &safe_name);
        let mut out = tokio::fs::File::create(&target).await?;
        let mut remaining = fmeta.size;
        while remaining > 0 {
            let want = std::cmp::min(remaining as usize, buf.len());
            let n = tls.read(&mut buf[..want]).await?;
            if n == 0 {
                return Err(anyhow!("connection closed mid-transfer"));
            }
            out.write_all(&buf[..n]).await?;
            remaining -= n as u64;
            bytes_recv += n as u64;
            if bytes_recv % (256 * 1024) < CHUNK_SIZE as u64 || bytes_recv == total_bytes {
                let mut t = state.active_transfers.read().get(&transfer_id).cloned().unwrap();
                t.current_file_index = idx;
                t.bytes_sent = bytes_recv;
                t.status = TransferStatus::Active;
                state.update_transfer(t);
            }
        }
        out.flush().await?;
    }

    let mut t = state.active_transfers.read().get(&transfer_id).cloned().unwrap();
    t.status = TransferStatus::Completed;
    t.bytes_sent = t.total_bytes;
    state.update_transfer(t);
    Ok(())
}

// -------- TLS configs --------

fn server_tls_config(state: &Arc<AppState>) -> Result<ServerConfig> {
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut Cursor::new(state.identity.cert_pem.as_bytes()))
            .collect::<std::result::Result<_, _>>()?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut Cursor::new(
        state.identity.key_pem.as_bytes(),
    ))?
    .ok_or_else(|| anyhow!("no private key"))?;
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(cfg)
}

fn client_tls_config() -> Result<ClientConfig> {
    let cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerify))
        .with_no_client_auth();
    Ok(cfg)
}

#[derive(Debug)]
struct NoCertVerify;

impl ServerCertVerifier for NoCertVerify {
    fn verify_server_cert(
        &self,
        _end: &CertificateDer<'_>,
        _ints: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        // Match defaults from rustls
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

// Silence unused-import warning for RootCertStore (kept for future pinning).
#[allow(dead_code)]
fn _root_cert_store_placeholder() -> RootCertStore {
    RootCertStore::empty()
}

// -------- Framing helpers --------

async fn write_msg<S: AsyncWriteExt + Unpin>(s: &mut S, body: &[u8]) -> Result<()> {
    let len = body.len() as u32;
    s.write_all(&len.to_be_bytes()).await?;
    s.write_all(body).await?;
    Ok(())
}

async fn read_msg<S: AsyncReadExt + Unpin>(s: &mut S) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    s.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_HEADER_BYTES {
        return Err(anyhow!("header too large: {} bytes", len));
    }
    let mut buf = vec![0u8; len as usize];
    s.read_exact(&mut buf).await?;
    Ok(buf)
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
        out.push_str("file");
    }
    // strip leading dots to avoid hidden files surprises
    let trimmed = out.trim_start_matches('.');
    if trimmed.is_empty() {
        "file".into()
    } else {
        trimmed.to_string()
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

// -------- Public: contact request to a peer --------

pub async fn send_contact_request(
    state: Arc<AppState>,
    peer_id: String,
    message: Option<String>,
) -> Result<()> {
    let peer = state
        .peers
        .read()
        .get(&peer_id)
        .cloned()
        .context("peer not found")?;
    let (name, avatar) = {
        let s = state.settings.read();
        (s.device_name.clone(), s.device_avatar.clone())
    };
    let connector = TlsConnector::from(Arc::new(client_tls_config()?));
    let stream = TcpStream::connect((peer.host.as_str(), peer.transfer_port)).await?;
    let dns = ServerName::try_from("localbeam.local").unwrap().to_owned();
    let mut tls = connector.connect(dns, stream).await?;
    let header = HeaderMsg::ContactRequest {
        from: PeerIdent {
            id: state.identity.fingerprint.clone(),
            name,
            avatar,
        },
        message,
    };
    write_msg(&mut tls, &serde_json::to_vec(&header)?).await?;
    let _resp = read_msg(&mut tls).await?;
    Ok(())
}

// -------- Decision plumbing (called from a tauri command) --------

pub fn decide_pending(state: &Arc<AppState>, transfer_id: &str, accept: bool) {
    if let Some(tx) = state.pending_decisions.write().remove(transfer_id) {
        let _ = tx.send(accept);
    }
}
