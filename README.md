# Televizion desktop

The Windows app for [Televizion](http://televizion.tv): the web player in a small native window
(Tauri, using the system's WebView2) plus a transcoder that runs on your own computer, so movies
in formats a browser cannot play (MKV, AVI, AC3/DTS sound) are converted locally with ffmpeg.

**Download:** the latest installer is on the [Releases](../../releases/latest) page.
The app checks for new versions by itself and offers to install them.

- `src-tauri/` — the app: starts the transcoder on a free local port, opens the window, checks for updates.
- `transcoder-rs/` — the transcoder: one Rust executable that wraps ffmpeg/ffprobe and serves HLS on 127.0.0.1.
- `ui/` — placeholder page bundled with the app.

Nothing is converted on a server: the transcoder listens on loopback only and accepts requests
from the Televizion page only. Updates are signed; the app refuses an installer that was not
produced by this repository's build.

ffmpeg is bundled unmodified from [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds) (GPL v3).
