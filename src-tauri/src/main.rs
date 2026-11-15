// Televizion desktop app.
//
// A small native window (the system's WebView2, no bundled browser) that shows the regular web
// player, plus the transcoder running on this computer so videos the browser cannot play are
// converted locally with the bundled ffmpeg.
//
// Bundled next to the executable (see tauri.conf.json → bundle.resources, filled by the CI job):
//   resources/transcoder/televizion-transcoder.exe   the transcoder (Rust, desktop/transcoder-rs)
//   resources/ffmpeg/                                ffmpeg.exe, ffprobe.exe and their DLLs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod transcoder;
mod update;

use std::path::PathBuf;
use std::process::Child;
use std::sync::Mutex;

use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

/// The page shown in the window.
const SITE: &str = "http://televizion.tv/browse/";

/// The running transcoder, so it can be stopped when the app exits.
pub struct Transcoder(pub Mutex<Option<Child>>);

fn resources(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = app.path().resource_dir().ok()?.join("resources");
    // Windows hands out "\\?\C:\…" paths; ffmpeg is happier with plain ones.
    Some(dunce::simplified(&dir).to_path_buf())
}

/// Starts the transcoder. Returns the process and its address ("" when it is not running, so
/// the page shows its "transcoder not running" message instead of calling a dead port).
fn start_transcoder(app: &tauri::AppHandle) -> (Option<Child>, String) {
    let Some(res) = resources(app) else { return (None, String::new()) };
    // Its log goes to the app's data folder (…\AppData\Roaming\tv.televizion.desktop\transcoder.log).
    let log_dir = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = dunce::simplified(&log_dir).join("transcoder.log");
    match transcoder::start(&res, &log_file) {
        Some(started) => {
            let address = started.port.map(|port| format!("http://127.0.0.1:{port}")).unwrap_or_default();
            (Some(started.child), address)
        }
        None => (None, String::new()),
    }
}

fn main() {
    let app = tauri::Builder::default()
        // Starting the app twice brings the existing window forward instead (one transcoder, one port).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Waits (10 s at most, normally a fraction of a second) for the transcoder's port.
            let (child, transcoder_address) = start_transcoder(app.handle());
            app.manage(Transcoder(Mutex::new(child)));

            // Tells the web page it runs inside the desktop app and where the transcoder is
            // (assets/js/app.js reads window.televizionDesktop).
            let init = format!(
                "window.televizionDesktop = Object.freeze({{ version: '{}', transcoder: '{}' }});",
                app.package_info().version,
                transcoder_address
            );

            let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(SITE.parse()?))
                .title("Televizion")
                .inner_size(1360.0, 860.0)
                .min_inner_size(720.0, 480.0)
                .initialization_script(&init);
            // The page is served over http and calls http://127.0.0.1:<port>. Chromium blocks
            // public → local requests from non-secure pages unless these features are off.
            // (The first three are Tauri's own defaults, which this option replaces.)
            #[cfg(windows)]
            let builder = builder.additional_browser_args(
                "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,BlockInsecurePrivateNetworkRequests,PrivateNetworkAccessSendPreflights --autoplay-policy=no-user-gesture-required",
            );
            builder.build()?;
            // Looks for a newer version now and every few hours; asks before installing it.
            update::watch(app.handle().clone());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the Televizion app");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            if let Some(mut child) = handle.state::<Transcoder>().0.lock().unwrap().take() {
                let _ = child.kill();
            }
        }
    });
}
