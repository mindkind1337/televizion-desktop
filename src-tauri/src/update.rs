// Updates.
//
// The app asks GitHub (the public televizion-desktop repository, see tauri.conf.json → plugins.updater)
// whether a newer version exists: at start, then every six hours. When there is one it says so in a
// dialog; on "yes" it downloads the signed installer, stops the transcoder (its files are replaced by
// the installer) and installs. Updates are verified against the public key in tauri.conf.json, so a
// file that was not signed by the Televizion build is refused.
use std::time::Duration;

use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;

const RECHECK: Duration = Duration::from_secs(6 * 60 * 60);

pub fn watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // Let the window come up first.
        tokio_sleep(Duration::from_secs(8)).await;
        let mut declined: Option<String> = None; // version the user said "later" to, during this run
        loop {
            check(&app, &mut declined).await;
            tokio_sleep(RECHECK).await;
        }
    });
}

async fn tokio_sleep(d: Duration) {
    // No direct tokio dependency: sleep on a blocking thread of the async runtime.
    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(d)).await;
}

async fn check(app: &AppHandle, declined: &mut Option<String>) {
    let Ok(updater) = app.updater() else { return };
    let update = match updater.check().await {
        Ok(Some(update)) => update,
        Ok(None) => {
            crate::log(app, "update check: up to date");
            return;
        }
        Err(err) => {
            crate::log(app, &format!("update check failed: {err}"));
            return;
        }
    };
    crate::log(app, &format!("update available: {} (running {})", update.version, update.current_version));
    if declined.as_deref() == Some(update.version.as_str()) {
        return;
    }

    let question = format!(
        "La version {new} de Televizion est disponible (vous avez la {old}).\nVoulez-vous l'installer maintenant ? L'application va red\u{e9}marrer.\n\nTelevizion {new} is available (you have {old}). Install it now? The app will restart.",
        new = update.version,
        old = update.current_version
    );
    // TVZ_AUTO_UPDATE=1 installs without asking (used by the automated test of the update path).
    let unattended = std::env::var("TVZ_AUTO_UPDATE").map(|v| v == "1").unwrap_or(false);
    let dialog_app = app.clone();
    let accepted = unattended || tauri::async_runtime::spawn_blocking(move || {
        dialog_app
            .dialog()
            .message(question)
            .title("Televizion")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom("Mettre \u{e0} jour".into(), "Plus tard".into()))
            .blocking_show()
    })
    .await
    .unwrap_or(false);
    if !accepted {
        *declined = Some(update.version.clone());
        return;
    }

    crate::log(app, "update: downloading");
    let bytes = match update.download(|_chunk, _total| {}, || {}).await {
        Ok(bytes) => bytes,
        Err(err) => {
            failed(app, &format!("{err}"));
            return;
        }
    };
    // The installer replaces the transcoder's executable: it must not be running.
    if let Some(mut child) = app.state::<crate::Transcoder>().0.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    crate::log(app, "update: installing");
    if let Err(err) = update.install(bytes) {
        failed(app, &format!("{err}"));
        return;
    }
    app.restart();
}

fn failed(app: &AppHandle, reason: &str) {
    crate::log(app, &format!("update failed: {reason}"));
    if std::env::var("TVZ_AUTO_UPDATE").map(|v| v == "1").unwrap_or(false) { return; }
    let app = app.clone();
    let text = format!("La mise \u{e0} jour n'a pas pu \u{ea}tre install\u{e9}e. / The update could not be installed.\n\n{reason}");
    let _ = tauri::async_runtime::spawn_blocking(move || {
        app.dialog().message(text).title("Televizion").kind(MessageDialogKind::Error).blocking_show()
    });
}
