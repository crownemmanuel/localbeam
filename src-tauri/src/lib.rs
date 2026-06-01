mod commands;
mod contacts;
mod discovery;
mod http_server;
mod identity;
mod settings;
mod state;
mod transfer;

use crate::{
    contacts::ContactBook, identity::Identity, settings::Settings, state::AppState,
};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow, Wry,
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

struct KeepOpenItem(CheckMenuItem<Wry>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Install rustls CryptoProvider once.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,localbeam_lib=debug".into()),
        )
        .with_target(false)
        .try_init()
        .ok();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            // Hide dock icon on macOS so only the tray icon represents the app.
            #[cfg(target_os = "macos")]
            {
                let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            let app_handle = app.handle().clone();
            let data_dir = app_handle
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("localbeam"));
            std::fs::create_dir_all(&data_dir).ok();

            let identity = Identity::load_or_create(&data_dir)
                .expect("failed to create/load identity");
            let settings = Settings::load_or_create(&data_dir)
                .expect("failed to load settings");
            let contacts = ContactBook::load_or_create(&data_dir)
                .expect("failed to load contacts");
            let state = AppState::new(data_dir, identity, settings, contacts);
            state.set_app_handle(app_handle.clone());
            app.manage(state.clone());

            // Tray
            if let Err(e) = build_tray(&app_handle) {
                tracing::error!(?e, "tray failed to build");
            }

            // Position the main window at bottom-right and show it so the user
            // has a visible entry point on first launch (in addition to the tray).
            if let Some(win) = app_handle.get_webview_window("main") {
                position_bottom_right(&win);
                let _ = win.show();
                let _ = win.set_focus();
                let h = app_handle.clone();
                let win_clone = win.clone();
                let st_blur = state.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(false) = event {
                        // Only auto-hide when not suspended (e.g. a native dialog is open).
                        if *st_blur.auto_hide.read() {
                            let _ = win_clone.hide();
                            let _ = h.emit("window-hidden", ());
                        }
                    }
                });
            }

            // Background runtimes
            let st_transfer = state.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = transfer::start_server(st_transfer).await {
                    tracing::error!(?e, "transfer server crashed");
                }
            });

            let st_http = state.clone();
            tauri::async_runtime::spawn(async move {
                if st_http.settings.read().enable_qr_server {
                    if let Err(e) = http_server::start_server(st_http).await {
                        tracing::error!(?e, "http server crashed");
                    }
                }
            });

            // UDP broadcast discovery — responder + periodic scanner
            let st_disc = state.clone();
            tauri::async_runtime::spawn(async move {
                discovery::start(st_disc).await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_me,
            commands::list_peers,
            commands::list_contacts,
            commands::list_pending_contact_requests,
            commands::list_transfers,
            commands::update_settings,
            commands::send_files,
            commands::send_contact_request,
            commands::decide_incoming,
            commands::accept_contact_request,
            commands::reject_contact_request,
            commands::remove_contact,
            commands::clear_completed_transfers,
            toggle_window,
            show_window,
            hide_window,
            republish_discovery,
            set_auto_hide,
            quit_app,
        ])
        .build(tauri::generate_context!())
        .expect("error while building localbeam")
        .run(|app, event| {
            // macOS: when the user re-launches the app (Spotlight, `open -a`,
            // double-clicking the bundle in Finder), bring the window back.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(w) = app.get_webview_window("main") {
                    position_bottom_right_once(&w);
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (app, event);
            }
        });
}


fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open_i = MenuItem::with_id(app, "open", "Open LocalBeam", true, None::<&str>)?;
    let keep_open_i = CheckMenuItem::with_id(app, "keep_open", "Keep Open", true, false, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_i, &keep_open_i, &quit_i])?;

    // Store the check item so the menu event handler can read/write its state.
    app.manage(KeepOpenItem(keep_open_i));

    let h = app.clone();
    let tray_icon = match tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png")) {
        Ok(img) => img,
        Err(e) => {
            tracing::warn!(?e, "failed to decode tray.png, falling back to window icon");
            app.default_window_icon()
                .expect("no default window icon to fall back to")
                .clone()
        }
    };

    let _tray = TrayIconBuilder::with_id("main-tray")
        .icon(tray_icon)
        .icon_as_template(false)
        .tooltip("LocalBeam")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, ev| match ev.id.as_ref() {
            "open" => {
                if let Some(w) = app.get_webview_window("main") {
                    position_bottom_right_once(&w);
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "keep_open" => {
                let state = app.state::<Arc<AppState>>();
                let item = app.state::<KeepOpenItem>();
                // Native CheckMenuItem toggles its own check state on click;
                // read the new state and sync auto_hide to the inverse.
                let is_checked = item.0.is_checked().unwrap_or(false);
                *state.auto_hide.write() = !is_checked;
                if is_checked {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |_tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let state = h.state::<Arc<AppState>>();
                if let Some(w) = h.get_webview_window("main") {
                    if *state.auto_hide.read() {
                        // Normal mode: left-click toggles visibility.
                        if w.is_visible().unwrap_or(false) {
                            let _ = w.hide();
                        } else {
                            position_bottom_right_once(&w);
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    } else {
                        // Keep-open mode: left-click only shows, never hides.
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
        })
        .build(app)?;
    Ok(())
}

static WINDOW_POSITIONED: AtomicBool = AtomicBool::new(false);

/// Position the window at the bottom-right corner, but only on the very first
/// call. After that the user's dragged position is preserved.
fn position_bottom_right_once(win: &WebviewWindow) {
    if WINDOW_POSITIONED.swap(true, Ordering::Relaxed) {
        return;
    }
    position_bottom_right(win);
}

fn position_bottom_right(win: &WebviewWindow) {
    let Ok(Some(monitor)) = win.current_monitor().or_else(|_| win.primary_monitor()) else {
        return;
    };
    let m_size: PhysicalSize<u32> = *monitor.size();
    let m_pos: PhysicalPosition<i32> = *monitor.position();
    let scale = monitor.scale_factor();

    let win_size = win
        .outer_size()
        .unwrap_or(PhysicalSize::new((380.0 * scale) as u32, (560.0 * scale) as u32));

    // 12px logical margin
    let margin_px = (12.0 * scale) as i32;
    let x = m_pos.x + m_size.width as i32 - win_size.width as i32 - margin_px;
    let y = m_pos.y + m_size.height as i32 - win_size.height as i32 - margin_px;
    // On macOS, the menubar inset is already excluded from monitor.size when using current_monitor.
    let _ = win.set_position(PhysicalPosition::new(x.max(m_pos.x), y.max(m_pos.y)));
}

#[tauri::command]
fn toggle_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            position_bottom_right_once(&w);
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

#[tauri::command]
fn show_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        position_bottom_right_once(&w);
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

#[tauri::command]
async fn republish_discovery(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    discovery::scan(state.inner()).await;
    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn set_auto_hide(state: tauri::State<'_, Arc<AppState>>, enabled: bool) {
    *state.auto_hide.write() = enabled;
}
