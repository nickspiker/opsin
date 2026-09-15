//! Single instance: `opsin file` while a viewer is already running loads the file into THAT window instead of spawning another. The first viewer binds a Unix socket in `$XDG_RUNTIME_DIR` (fallback `/tmp/opsin-<uid>.sock`); every later launch tries to connect first — success means hand the absolute path over and exit; refusal means no live instance (a stale socket from a crash is unlinked) and this process becomes the instance. The listener thread pushes each received path through fluor's wake-sender so it lands on the UI thread in `on_user_event`, which loads it like a drop and returns `ShowWindow` to raise the window. An empty line (bare `opsin`) just raises. Headless modes (`--convert`, `--check`, `--copy-idt`, `--paste-idt`) never touch this.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("opsin.sock"),
        _ => {
            // Not a config dir — a per-user runtime socket. UID from the effective user via `id -u` isn't available without libc; HOME's owner is close enough for the fallback.
            let uid = std::fs::metadata(std::env::var_os("HOME").unwrap_or_default()).map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(0);
            PathBuf::from(format!("/tmp/opsin-{uid}.sock"))
        }
    }
}

/// Try to hand `path` (or nothing — just raise) to a running instance. `true` ⇒ delivered, caller exits. `false` ⇒ no instance is listening, caller becomes it.
pub fn handoff(path: Option<&Path>) -> bool {
    let Ok(mut s) = UnixStream::connect(socket_path()) else { return false };
    let line = match path {
        Some(p) => std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).display().to_string(),
        None => String::new(),
    };
    s.write_all(format!("{line}\n").as_bytes()).is_ok()
}

/// Become the instance: bind the socket (unlinking a stale one) and serve it on a background thread, delivering each line to `deliver`. Returns the socket path for cleanup, or `None` if binding failed (then we're just an ordinary process — the viewer still runs).
pub fn listen(deliver: impl Fn(Option<PathBuf>) + Send + 'static) -> Option<PathBuf> {
    let sock = socket_path();
    // connect() already failed in handoff(), so anything at the path is a corpse.
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("opsin: single-instance socket {}: {e} (running standalone)", sock.display());
            return None;
        }
    };
    std::thread::Builder::new()
        .name("opsin-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_ok() {
                    let line = line.trim_end_matches(['\n', '\r']);
                    deliver(if line.is_empty() { None } else { Some(PathBuf::from(line)) });
                }
            }
        })
        .ok()?;
    Some(sock)
}
