// Starts the bundled transcoder (resources/transcoder/televizion-transcoder) and learns which
// port it listens on.
//
// The transcoder asks for port 3002 but takes any free port when that one is refused (Windows
// reserves port ranges for Hyper-V/WSL), and prints `TVZ_LISTENING <port>` once it listens.
// Only the standard library is used here.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// The port asked for first (the transcoder falls back to a free one by itself).
const PREFERRED_PORT: &str = "3002";
/// How long to wait for the transcoder to say where it listens.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Started {
    /// Kept so the process can be stopped when the app exits.
    pub child: Child,
    /// None when the transcoder did not report a port in time.
    pub port: Option<u16>,
}

/// `TVZ_LISTENING 3002` → 3002
pub fn parse_listening(line: &str) -> Option<u16> {
    line.trim().strip_prefix("TVZ_LISTENING ")?.trim().parse().ok()
}

/// `resources`: the app's bundled resources folder. `log_file`: where the transcoder writes its
/// own log (it truncates the file at each start and caps its size).
pub fn start(resources: &Path, log_file: &Path) -> Option<Started> {
    let exe = resources.join("transcoder").join(if cfg!(windows) { "televizion-transcoder.exe" } else { "televizion-transcoder" });
    if !exe.exists() {
        eprintln!("transcoder missing: {}", exe.display());
        return None;
    }

    // ffmpeg and ffprobe are found through PATH.
    let mut paths = vec![resources.join("ffmpeg")];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let path = std::env::join_paths(paths).ok()?;

    let mut cmd = Command::new(&exe);
    cmd.current_dir(resources.join("transcoder"))
        .env("TVZ_DESKTOP", "1") // loopback only, requests from the Televizion site only
        .env("TVZ_PORT", PREFERRED_PORT)
        .env("TVZ_STREAMS_DIR", std::env::temp_dir().join("televizion-streams"))
        .env("TVZ_LOG_FILE", log_file)
        .env("PATH", path)
        // Kept open and never written to: when the app ends (even killed), the pipe closes and the
        // transcoder stops itself, taking its ffmpeg processes with it.
        .env("TVZ_EXIT_WITH_PARENT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()); // everything it prints is also in its log file
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console window behind the app
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("could not start the transcoder: {err}");
            return None;
        }
    };

    // Read its output on a thread: the first `TVZ_LISTENING` line gives the port; the rest is
    // read and dropped for as long as it runs, so the pipe never fills up.
    let (tx, rx) = mpsc::channel::<u16>();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut tx = Some(tx);
            for line in BufReader::new(stdout).split(b'\n') {
                let Ok(line) = line else { break };
                if let Some(port) = parse_listening(&String::from_utf8_lossy(&line)) {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(port);
                    }
                }
            }
        });
    }
    let port = rx.recv_timeout(STARTUP_TIMEOUT).ok();
    if port.is_none() {
        eprintln!("the transcoder did not report a port (see {})", log_file.display());
    }
    Some(Started { child, port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_line() {
        assert_eq!(parse_listening("TVZ_LISTENING 3002"), Some(3002));
        assert_eq!(parse_listening("TVZ_LISTENING 49731\r"), Some(49731));
        assert_eq!(parse_listening("Transcode listening on 3002"), None);
        assert_eq!(parse_listening("TVZ_LISTENING x"), None);
    }
}
