//! opsin — spectral image viewer/converter (single binary).
//!
//!   opsin                     open an empty viewer; drag any supported image onto it.
//!   opsin <file>              open the viewer on an image (any supported format); ←/→ navigate the folder, V converts the current image to VSF.
//!   opsin <dir>               open the first supported image in a directory.
//!   opsin --convert <in> [out]  headless: decode <in> to a VSF-Image (default <in>.vsf). The GUI's V without a window.

mod app;
mod convert;
mod panel;
mod state;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
                Ok(dec) => match convert::write_vsf(&dec.img, &out) {
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
                    match convert::to_linear(&dec, false) {
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
            let viewer = match app::OpsinApp::open(path.as_ref()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("opsin: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = fluor::host::app::run_app(viewer) {
                eprintln!("opsin: event loop: {e}");
                std::process::exit(1);
            }
        }
        None => {
            // No argument: empty drop-target viewer.
            if let Err(e) = fluor::host::app::run_app(app::OpsinApp::empty()) {
                eprintln!("opsin: event loop: {e}");
                std::process::exit(1);
            }
        }
    }
}
