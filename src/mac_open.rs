//! Finder opens on macOS: the document arrives as an Apple Event, NEVER in argv. Double-clicking an image hands `Opsin.app` a `kAEOpenDocuments` ('odoc') event and launches it with `argc == 1` — measured, not assumed — so the argv path in `main` sees nothing and the window comes up empty. Linux's equivalent is the `.desktop` `%f` argument, which IS argv, which is why nothing like this file is needed there. This is the macOS half of `instance.rs`: same destination (`Msg::Open` → `on_user_event` → `show_path` + raise), different transport. It covers BOTH openings — the launch document and every later double-click — because Launch Services never spawns the second process the socket exists to catch; it routes straight into the running app.
//!
//! THE MECHANISM, and why it is this one. AppKit installs its own 'odoc' handler inside `NSApplication::finishLaunching` and processes the queued launch document right there, forwarding it to the application delegate. Two other routes were tried against a real bundle, and each lost the LAUNCH document while later double-clicks still worked — which surfaces as Finder refusing to open the file at all, but only when opsin was not already running:
//!
//! 1. Claiming the event at the Carbon `AEInstallEventHandler` layer. Installed before the event loop or from `FluorApp::init`, it makes no difference: `finishLaunching` installs over the handler and consumes the launch document before `init` is ever reached.
//! 2. Registering our own `NSApplicationDelegate`, which `winit::platform::macos` documents as the supported path ("Winit guarantees that it will not register an application delegate"). That guarantee does not hold in winit 0.30.13, which registers `WinitApplicationDelegate` and aborts the process the moment AppKit hands it an event — "tried to get a delegate that was not the one Winit has registered", `app_state.rs:182`.
//!
//! So the delegate stays winit's, and the method is added to winit's delegate CLASS at runtime instead. AppKit asks the delegate whether it responds to `application:openURLs:`, and by then it does — adding the method is the whole fix, verified on a cold launch; no re-registration or cache poking is needed. The cost is a dependency on winit's private class name, so [`install`] reports and degrades to drop-and-argv-only if a winit upgrade renames it, rather than failing silently or taking the window down.
//!
//! TIMING: the class must gain the method after `EventLoop::new` (which is what creates it) and before `run_app` (which is what lets `finishLaunching` run) — exactly the window fluor hands us in `FluorApp::set_event_proxy`, which is also where the wake-sender arrives. One call does both.

use std::path::PathBuf;
use std::sync::Mutex;

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{ffi, sel};
use objc2_foundation::{NSArray, NSURL};

/// winit's application delegate, by the name winit gives it in `platform_impl/macos/app_state.rs`.
const WINIT_DELEGATE_CLASS: &std::ffi::CStr = c"WinitApplicationDelegate";

/// ObjC type encoding for `application:openURLs:` — void return, the two implicit arguments (self, selector), then the NSApplication and the NSArray.
const OPEN_URLS_ENCODING: &std::ffi::CStr = c"v@:@@";

/// Where a delivered path goes. Set by [`install`] BEFORE the method is added, so an opening cannot arrive ahead of it.
static SINK: Mutex<Option<Box<dyn Fn(PathBuf) + Send>>> = Mutex::new(None);

/// The added method. Every Finder open lands here — the launch document and each later double-click alike — on the main thread, so the sink's `send` reaches the wake-sender exactly as the socket listener's does.
extern "C" fn application_open_urls(
    _delegate: &AnyObject,
    _cmd: Sel,
    _app: &AnyObject,
    urls: &NSArray<NSURL>,
) {
    let Ok(sink) = SINK.lock() else { return };
    let Some(sink) = sink.as_ref() else { return };
    for i in 0..urls.count() {
        // `path` is None for a URL that names no file; nothing but files is registered to us, but the type permits it.
        if let Some(path) = unsafe { urls.objectAtIndex(i).path() } {
            sink(PathBuf::from(path.to_string()));
        }
    }
}

/// Route Finder's opens into `sink`. Call from `set_event_proxy` — see the module note on timing.
pub fn install(sink: impl Fn(PathBuf) + Send + 'static) {
    // The sink goes in first: the method becomes callable the instant it is added.
    if let Ok(mut cell) = SINK.lock() {
        *cell = Some(Box::new(sink));
    }
    let Some(class) = AnyClass::get(WINIT_DELEGATE_CLASS) else {
        eprintln!(
            "opsin: no `{}` class — winit changed its delegate, so Finder opens can't be routed (drag-and-drop and `opsin <file>` still work)",
            WINIT_DELEGATE_CLASS.to_string_lossy()
        );
        return;
    };
    // SAFETY: the IMP is called with exactly the arguments `OPEN_URLS_ENCODING` describes, which is AppKit's signature for this selector. The transmute is how a typed `extern "C" fn` is handed to the runtime, which types every IMP as nullary.
    let imp: Imp = unsafe {
        std::mem::transmute(
            application_open_urls as extern "C" fn(&AnyObject, Sel, &AnyObject, &NSArray<NSURL>),
        )
    };
    let added = unsafe {
        ffi::class_addMethod(
            (class as *const AnyClass).cast_mut().cast(),
            sel!(application:openURLs:),
            imp,
            OPEN_URLS_ENCODING.as_ptr(),
        )
    };
    if !added.as_bool() {
        // Only possible if the class already defines the selector — a winit that grew its own document-open support, which would mean this module should go away rather than fight it.
        eprintln!(
            "opsin: `{}` already implements application:openURLs: — leaving winit's to it",
            WINIT_DELEGATE_CLASS.to_string_lossy()
        );
    }
}
