//! opsin — spectral image viewer/converter (single binary).
//!
//!   opsin                     open an empty viewer; drag any supported image onto it.
//!   opsin <file>              open the viewer on an image (any supported format); ←/→ navigate the folder, V converts the current image to VSF.
//!   opsin <dir>               open the first supported image in a directory.
//!   opsin --convert <in> [out]  headless: decode <in> to a VSF-Image (default <in>.vsf). The GUI's V without a window.
//!   opsin --copy-idt <frame>    lift the DSR IDT (DNG ColorMatrix1 + illuminant + camera fingerprint) into the clip file. The GUI's Ctrl+C.
//!   opsin --live               headless virtual webcam (live feature): camera → saved matrix → /dev/video10, no window. Ctrl+C stops.
//!   opsin --paste-idt [--force] <frame>...  write the clipped IDT into each frame — as a DNG CameraCalibration onto the file's own matrix, with the class and provenance in XMP; nothing original moves — only where the camera fingerprint matches; --force overrides a mismatch (a lens change on an interchangeable-lens body) with a warning. The GUI's Ctrl+V / Ctrl+Shift+V, for a whole folder.
//!   opsin --paste-idt <folder>...  a folder pastes onto every DNG / VSF directly inside it (not recursive).
//!   opsin --copy-idt / --paste-idt --notify …  the file manager's right-click form: results as a notification, failures as a dialog, and a camera mismatch asks "Paste anyway?" instead of only refusing.
//!   opsin --unpaste-idt <frame>...  undo every paste on each frame: the file comes back byte for byte.
//!   opsin --thumbnail <in> <out.png> [size]  headless: the file manager's thumbnailer (`%i %o %s`) — the image as it opens, sRGB PNG, long edge ≤ size (default 256).

mod app;
mod view;
#[cfg(feature = "live")]
mod live;
#[cfg(feature = "calibrate")]
mod calibrate;
mod convert;
mod debayer;
mod headerless;
/// Finder's document opens arrive as an Apple Event, not as argv — see the module note. macOS only; Linux gets its paths from the `.desktop` `%f` and needs nothing here.
#[cfg(target_os = "macos")]
mod mac_open;
mod sniff;
mod shell;
mod icc;
mod idt;
mod instance;
mod tiff;
mod panel;
mod state;

/// Panic hook: mirror every panic to `<tmp>/opsin-panic.log` before the default hook prints it. Desktop launches have no terminal — without this a panicking open is indistinguishable from nothing happening.
fn install_panic_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let log = std::env::temp_dir().join("opsin-panic.log");
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(&log).map(|mut f| {
            use std::io::Write;
            let _ = writeln!(f, "[unix {secs}] opsin {}: {info}", env!("CARGO_PKG_VERSION"));
        });
        default(info);
    }));
}

/// Run the viewer as THE instance. fluor exits the process from inside its loop on window close (`process::exit`), so the socket is unlinked by an atexit-style guard registered here, not after `run_app` returns; a crash leaves a corpse that the next launch's failed connect() unlinks anyway.
fn run_viewer(viewer: app::OpsinApp) {
    extern "C" fn unlink_socket() {
        if let Some(p) = app::OpsinApp::socket_cell().lock().ok().and_then(|s| s.clone()) {
            let _ = std::fs::remove_file(p);
        }
    }
    unsafe extern "C" {
        fn atexit(cb: extern "C" fn()) -> i32;
    }
    unsafe {
        atexit(unlink_socket);
    }
    if let Err(e) = fluor::host::app::run_app(viewer) {
        eprintln!("opsin: event loop: {e}");
        std::process::exit(1);
    }
}

/// SIGINT → the closure, once. libc-free: `signal(2)` with a static hook; only used by the headless `--live` loop to stop cleanly.
#[cfg(feature = "live")]
fn ctrlc_handler(f: impl Fn() + Send + Sync + 'static) -> Result<(), ()> {
    static HOOK: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();
    extern "C" fn on_sigint(_: i32) {
        if let Some(h) = HOOK.get() {
            h();
        }
    }
    unsafe extern "C" {
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    }
    HOOK.set(Box::new(f)).map_err(|_| ())?;
    unsafe {
        signal(2, on_sigint);
    }
    Ok(())
}

fn main() {
    install_panic_log();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `--notify` anywhere: report thru desktop notifications and dialogs instead of only stdout/stderr — what the file manager's right-click actions pass, since nothing reads their terminal.
    let notify = args.iter().any(|a| a == "--notify");
    args.retain(|a| a != "--notify");
    match args.first().map(String::as_str) {
        Some("--version") => println!("opsin {}", env!("CARGO_PKG_VERSION")),
        Some("--convert") => {
            let Some(input) = args.get(1) else {
                eprintln!("opsin --convert <in> [out]");
                std::process::exit(2);
            };
            let in_path = std::path::Path::new(input);
            let out = args.get(2).map(std::path::PathBuf::from).unwrap_or_else(|| in_path.with_extension("vsf"));
            match convert::load_any(in_path) {
                Ok(dec) => match convert::write_vsf(&dec, &out) {
                    Ok(()) => println!("opsin: wrote {}", out.display()),
                    Err(e) => {
                        eprintln!("opsin: convert: {e}");
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("opsin: {input}: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("--thumbnail") => {
            let (Some(input), Some(out)) = (args.get(1), args.get(2)) else {
                eprintln!("opsin --thumbnail <in> <out.png> [size]");
                std::process::exit(2);
            };
            let size = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(256);
            if let Err(e) = convert::write_thumbnail(input.as_ref(), size, out.as_ref()) {
                eprintln!("opsin: thumbnail {input}: {e}");
                std::process::exit(1);
            }
        }
        Some("--copy-idt") => {
            let Some(input) = args.get(1) else {
                eprintln!("opsin --copy-idt [--notify] <frame>");
                std::process::exit(2);
            };
            if notify && args.len() > 2 {
                shell::alert("Copy IDT", "Select one frame to copy the IDT from.");
                std::process::exit(2);
            }
            match idt::IdtClip::copy_from(std::path::Path::new(input)).and_then(|c| c.save().map(|p| (c, p))) {
                Ok((clip, p)) => {
                    print!("{}", clip.to_text());
                    println!("opsin: clip saved to {}", p.display());
                    if notify {
                        let fp = &clip.fingerprint;
                        shell::notify("IDT copied", &format!("{} {} ({} {}) from {}", fp.make, fp.model, clip.provenance.class, clip.provenance.tier, shell::name(std::path::Path::new(input))));
                    }
                }
                Err(e) => {
                    eprintln!("opsin: copy-idt: {e}");
                    if notify {
                        shell::alert("Copy IDT failed", &e);
                    }
                    std::process::exit(1);
                }
            }
        }
        Some("--paste-idt") => {
            let force = args.get(1).map(String::as_str) == Some("--force");
            let targets = shell::idt_targets(&args[if force { 2 } else { 1 }..]);
            if targets.is_empty() {
                if notify {
                    shell::alert("Paste IDT", "No frames that can carry an IDT (DNG or VSF) in the selection.");
                } else {
                    eprintln!("opsin --paste-idt [--force] [--notify] <frame|folder>...");
                }
                std::process::exit(2);
            }
            let clip = match idt::IdtClip::load() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("opsin: paste-idt: {e}");
                    if notify {
                        shell::alert("Paste IDT failed", &e);
                    }
                    std::process::exit(1);
                }
            };
            // Every target is judged on its own fingerprint; a refusal names the differing fields and moves on, so a mixed folder pastes onto exactly the frames it should.
            let mut pasted = 0;
            let mut forced = 0;
            let mut mismatched: Vec<std::path::PathBuf> = Vec::new();
            let mut failed: Vec<String> = Vec::new();
            let mut paste = |target: &std::path::Path, force: bool, mismatched: &mut Vec<std::path::PathBuf>| match clip.paste_into(target, force) {
                Ok(report) => {
                    println!("opsin: {report}");
                    pasted += 1;
                    if !report.forced_over.is_empty() {
                        forced += 1;
                    }
                }
                Err(e) => {
                    eprintln!("opsin: paste-idt: {e}");
                    if !force && e.contains(idt::MISMATCH) {
                        mismatched.push(target.to_path_buf());
                    } else {
                        failed.push(format!("{}: {}", shell::name(target), e.strip_prefix(&format!("{}: ", target.display())).unwrap_or(&e)));
                    }
                }
            };
            for target in &targets {
                paste(target, force, &mut mismatched);
            }
            // Without a terminal the refusals would vanish, so the file-manager paste asks: the frames from another body get one deliberate click, and a forced paste is still on record in each file.
            if notify && !mismatched.is_empty() {
                let fp = &clip.fingerprint;
                let names: Vec<String> = mismatched.iter().map(|p| shell::name(p)).collect();
                let body = format!(
                    "{} of {} frames are from a different camera than the copied IDT ({} {}):\n\n{}\nPaste anyway? Each forced paste is recorded in the file, and opsin --unpaste-idt undoes it.",
                    mismatched.len(), targets.len(), fp.make, fp.model, shell::listing(&names, 8)
                );
                if shell::confirm("Paste IDT — camera mismatch", &body, "Paste anyway") {
                    for target in std::mem::take(&mut mismatched) {
                        paste(&target, true, &mut Vec::new());
                    }
                }
            }
            drop(paste);
            if notify {
                let skipped = if mismatched.is_empty() { String::new() } else { format!(", {} skipped (different camera)", mismatched.len()) };
                let forced = if forced == 0 { String::new() } else { format!(", {forced} forced") };
                if failed.is_empty() {
                    shell::notify("IDT pasted", &format!("{pasted} of {} frames{forced}{skipped}", targets.len()));
                } else {
                    shell::alert("Paste IDT", &format!("{pasted} of {} frames pasted{forced}{skipped}.\n\n{} could not take the IDT:\n{}", targets.len(), failed.len(), shell::listing(&failed, 8)));
                }
            }
            if !failed.is_empty() || !mismatched.is_empty() {
                std::process::exit(1);
            }
        }
        Some("--unpaste-idt") => {
            let targets = &args[1..];
            if targets.is_empty() {
                eprintln!("opsin --unpaste-idt <frame>...");
                std::process::exit(2);
            }
            let mut failed = 0;
            for target in targets {
                match idt::IdtClip::unpaste(std::path::Path::new(target)) {
                    Ok(report) => println!("opsin: {report}"),
                    Err(e) => {
                        eprintln!("opsin: unpaste-idt: {e}");
                        failed += 1;
                    }
                }
            }
            if failed > 0 {
                std::process::exit(1);
            }
        }
        #[cfg(feature = "live")]
        Some("--live") => {
            // Headless virtual webcam: the saved matrix, the camera → loopback stream, no window, no viewfinder. Ctrl+C stops.
            let shared = live::Shared::new(live::load_matrix().unwrap_or(live::default_matrix()));
            shared.viewfinder.store(false, std::sync::atomic::Ordering::Relaxed);
            let handle = match live::start(shared.clone(), |m| {
                if let live::LiveMsg::Status(s) = m {
                    eprintln!("opsin: {s}");
                }
            }) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("opsin: live: {e}");
                    std::process::exit(1);
                }
            };
            let stop = shared.clone();
            let _ = ctrlc_handler(move || stop.stop.store(true, std::sync::atomic::Ordering::Relaxed));
            let _ = handle.join();
        }
        Some("--check") => {
            // Headless: run the full open() (load + colour + panel tools) and report, without a window. Times the decode and render stages.
            let path = args.get(1).map(String::as_str).unwrap_or("");
            eprintln!("check: opening {path}");
            let t0 = std::time::Instant::now();
            match convert::load_any(path.as_ref()) {
                Ok(dec) => {
                    let t_decode = t0.elapsed();
                    let tu = std::time::Instant::now();
                    let unpacked = dec.img.samples.unpack_u16();
                    eprintln!("check: unpack {:?} ({} samples)", tu.elapsed(), unpacked.len());
                    drop(unpacked);
                    let t1 = std::time::Instant::now();
                    match convert::to_linear(&dec) {
                        Ok((w, h, lin)) => {
                            let sum: i64 = lin.iter().map(|&v| v as i64).sum();
                            eprintln!("check: decode {t_decode:?}, to_linear {:?} ({w}×{h}, checksum {sum:x})", t1.elapsed());
                        }
                        Err(e) => {
                            eprintln!("check: to_linear ERR: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("check: load ERR: {e}");
                    std::process::exit(1);
                }
            }
            match app::OpsinApp::open(path.as_ref()) {
                Ok(_) => eprintln!("check: open() OK"),
                Err(e) => {
                    eprintln!("check: open() ERR: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(path) => {
            // Single instance: a running viewer takes the file; this process just delivered it.
            if instance::handoff(Some(std::path::Path::new(path))) {
                return;
            }
            // A failed (or panicking) open still gets a window: fall back to the empty drop-target viewer so a desktop launch shows SOMETHING rather than silently dying. The error goes to stderr and, for panics, the hook's log file.
            let viewer = match std::panic::catch_unwind(|| app::OpsinApp::open(path.as_ref())) {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    eprintln!("opsin: {e}");
                    app::OpsinApp::empty()
                }
                Err(_) => {
                    eprintln!("opsin: panicked opening {path} (see opsin-panic.log in the temp dir)");
                    app::OpsinApp::empty()
                }
            };
            run_viewer(viewer);
        }
        None => {
            // No argument: raise a running viewer if there is one, else the empty drop-target viewer.
            if instance::handoff(None) {
                return;
            }
            run_viewer(app::OpsinApp::empty());
        }
    }
}

































