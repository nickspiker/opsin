//! The file manager's right-click IDT actions (macOS Quick Actions, Dolphin / Nautilus / Nemo on Linux) — no terminal behind them, so this is where a batch meets the user: folders expand to the frames inside, and the outcome comes back as a notification or a dialog instead of stderr.
//!
//! Dialogs go thru whatever the desktop already has — `osascript` on macOS, `zenity` or `kdialog` on Linux, `notify-send` as the last resort — so nothing new enters the dependency graph. A missing dialog tool degrades to stderr, never to a failed paste.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The paste targets named by `args`: a file as given, a folder as every frame directly inside it that could carry an IDT (the TIFF family and VSF, by signature), sorted by name. Not recursive — a folder of shoots is one right-click per shoot, so a stray subfolder is never touched. Dotfiles are skipped (`._` AppleDouble shadows on exFAT cards sniff as nothing, but they are never frames either way).
pub fn idt_targets(args: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in args {
        let p = Path::new(a);
        if !p.is_dir() {
            out.push(p.to_path_buf());
            continue;
        }
        let Ok(rd) = std::fs::read_dir(p) else {
            out.push(p.to_path_buf()); // let the paste report the unreadable folder
            continue;
        };
        let mut frames: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|f| !f.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')))
            .filter(|f| matches!(crate::sniff::sniff_path(f), Some(crate::sniff::Kind::Tiff | crate::sniff::Kind::Vsf)))
            .collect();
        frames.sort();
        out.extend(frames);
    }
    out
}

/// A file's name for a dialog line — the folder is already in the user's head.
pub fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Up to `max` lines of `items`, then "+N more" — a 400-frame folder must not make a 400-line dialog.
pub fn listing(items: &[String], max: usize) -> String {
    let mut s: String = items.iter().take(max).map(|i| format!("  {i}\n")).collect();
    if items.len() > max {
        s.push_str(&format!("  +{} more\n", items.len() - max));
    }
    s
}

/// A transient "it worked" — never blocks.
pub fn notify(title: &str, body: &str) {
    eprintln!("opsin: {title}: {body}");
    #[cfg(target_os = "macos")]
    let _ = osascript(&["display notification (item 2 of argv) with title (item 1 of argv)"], &[title, body]);
    #[cfg(not(target_os = "macos"))]
    let _ = Command::new("notify-send").args(["--app-name=opsin", "--icon=opsin", title, body]).status();
}

/// A blocking "read this" — failures, which a notification would let scroll away.
pub fn alert(title: &str, body: &str) {
    eprintln!("opsin: {title}:\n{body}");
    #[cfg(target_os = "macos")]
    let _ = osascript(&["display dialog (item 2 of argv) with title (item 1 of argv) buttons {\"OK\"} default button 1 with icon caution"], &[title, body]);
    #[cfg(not(target_os = "macos"))]
    {
        let shown = run_ok(Command::new("zenity").args(["--warning", "--no-wrap", "--title", title, "--text", &pango_escape(body)]))
            || run_ok(Command::new("kdialog").args(["--title", title, "--sorry", body]));
        if !shown {
            let _ = Command::new("notify-send").args(["--app-name=opsin", "--urgency=critical", title, body]).status();
        }
    }
}

/// A yes/no with `yes` as the affirmative button's label; "Skip" is the default and the answer when no dialog tool exists — an override is only ever a deliberate click.
pub fn confirm(title: &str, body: &str, yes: &str) -> bool {
    eprintln!("opsin: {title}:\n{body}");
    #[cfg(target_os = "macos")]
    {
        osascript(&["button returned of (display dialog (item 2 of argv) with title (item 1 of argv) buttons {\"Skip\", item 3 of argv} default button \"Skip\" cancel button \"Skip\" with icon caution)"], &[title, body, yes]).is_some_and(|out| out.trim() == yes)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Both exit 0 for yes, 1 for no, and something else (or fail to spawn) when absent.
        if let Ok(s) = Command::new("zenity").args(["--question", "--no-wrap", "--title", title, "--text", &pango_escape(body), "--ok-label", yes, "--cancel-label", "Skip", "--default-cancel"]).status() {
            if s.code().is_some_and(|c| c <= 1) {
                return s.success();
            }
        }
        if let Ok(s) = Command::new("kdialog").args(["--title", title, "--yes-label", yes, "--no-label", "Skip", "--warningyesno", body]).status() {
            if s.code().is_some_and(|c| c <= 1) {
                return s.success();
            }
        }
        false
    }
}

/// Run an AppleScript with `args` as `argv` — text goes in as arguments, never spliced into the script, so a file name with quotes in it is just a file name. `Some(stdout)` on success.
#[cfg(target_os = "macos")]
fn osascript(lines: &[&str], args: &[&str]) -> Option<String> {
    let mut c = Command::new("osascript");
    c.args(["-e", "on run argv"]);
    for l in lines {
        c.args(["-e", l]);
    }
    c.args(["-e", "end run"]).args(args);
    let out = c.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn run_ok(c: &mut Command) -> bool {
    c.status().is_ok_and(|s| s.success())
}

/// zenity's `--text` is Pango markup: a `&` or `<` in a file name would otherwise blank the dialog.
#[cfg(not(target_os = "macos"))]
fn pango_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
