//! The opsin WINDOW — a fluor app that is a thin shell around [`crate::view::View`] (the viewer proper): window chrome + the top bar, window moves and resizes, folder navigation (←/→), drag-and-drop, the single-instance socket handoff, the IDT clipboard (file paths), and the `[]` debug chord. Everything the picture does — pan, zoom, crop, rotate, exposure, the panel, the HUD, the keys — lives in the view, which photon hosts too.

use fluor::canvas::Canvas;
use fluor::coord::Coord;
use fluor::event::{CursorIcon, ElementState, Event as FEvent, Key, MouseButton, NamedKey};
use fluor::geom::Viewport;
use fluor::host::app::{Context, EventResponse, FluorApp};
use fluor::host::chrome::{self, HIT_NONE, HitId, ResizeEdge};
use fluor::host::chrome_widget::DefaultChrome;
use fluor::host::widget::Container;
use fluor::paint::{self, Clip};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::view::{Area, HAIRLINE, Loaded, Msg, View, load_image};

/// Base tone (visible RGB) for the top-bar noise texture — the controls-strip grey (`WINDOW_CONTROLS_BG` ≈ 0x1E1E1E visible) so the textured fill and the flat control fill sit at the same value.
const BAR_TEXTURE_BASE: u32 = 0x00_1E_1E_1E;

/// Grace for X11's synthetic key-Release while a key is actually held (photon's chord constant).
const CHORD_RELEASE_GRACE: Duration = Duration::from_millis(40);

pub struct OpsinApp {
    title: String,
    chrome: DefaultChrome,
    view: View,
    /// Viewport dims mirrored from init/on_resize so hit_test_map (which has no Context) can report them.
    view_w: usize,
    view_h: usize,
    /// Supported images in the opened folder, sorted; ←/→ step through them.
    dir_list: Vec<PathBuf>,
    /// Index into `dir_list` of the image currently shown.
    dir_idx: usize,
    /// Total allocated HitIds (chrome + the view's pills and sliders) — the host's overlay tables are indexed by id, so their length is this + 1.
    hit_count: HitId,
    // --- [] debug chord (photon's scheme): both brackets held arms the chord, the next letter fires a debug toggle. Press/release Instants instead of booleans so X11's synthetic Release on the next keypress is absorbed by the grace window.
    chord_lb_press: Option<Instant>,
    chord_lb_release: Option<Instant>,
    chord_rb_press: Option<Instant>,
    chord_rb_release: Option<Instant>,
    /// Hitmask debug overlay active ([]h) — render's last act replaces every pixel with its hit id's palette colour.
    show_hitmask: bool,
    /// 256 random opaque colours (α+darkness), regenerated on each []h enable so distinct ids always pop.
    debug_hit_colours: Vec<u32>,
}

/// Sorted list of supported images in `dir`.
fn folder_images(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| crate::convert::is_supported(p)).collect();
    v.sort();
    v
}

/// The folder image list + the index of `path` within it, for arrow navigation. A directory yields its images at index 0; a file yields its folder's images positioned on itself (inserted at front if the scan didn't catch it).
fn dir_list_for(path: &Path) -> (Vec<PathBuf>, usize) {
    if path.is_dir() {
        return (folder_images(path), 0);
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut list = folder_images(dir);
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match list.iter().position(|p| std::fs::canonicalize(p).map(|c| c == canon).unwrap_or(false)) {
        Some(i) => (list, i),
        None => {
            list.insert(0, path.to_path_buf());
            (list, 0)
        }
    }
}

/// The single-instance socket path once bound (so main can unlink it on exit). A process-wide cell because the app is moved into the host's event loop and main never sees it again.
static SOCKET: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

impl OpsinApp {
    pub fn socket_cell() -> &'static std::sync::Mutex<Option<PathBuf>> {
        &SOCKET
    }

    /// Start with no image — an empty drop target. Drag any supported file onto the window (or it arrives via `show_path`); the panel's locus/Planck chart renders from the observer alone.
    pub fn empty() -> Self {
        Self::from_loaded(Loaded::empty(), Vec::new(), 0)
    }

    /// Open a file (shown, folder siblings navigable) or a directory (first supported image shown).
    pub fn open(path: &Path) -> Result<Self, String> {
        let (dir_list, dir_idx) = dir_list_for(path);
        if dir_list.is_empty() {
            return Err(format!("{}: no supported images", path.display()));
        }
        let loaded = load_image(&dir_list[dir_idx])?;
        Ok(Self::from_loaded(loaded, dir_list, dir_idx))
    }

    fn from_loaded(loaded: Loaded, dir_list: Vec<PathBuf>, dir_idx: usize) -> Self {
        // App orb — three cone-fundamental lobes blooming warm/cool from centre (the observer the app is named for). Bundled as a 256×256 VSF; regenerate with `cargo run --bin make_orb` after editing the art. Decode failure is non-fatal — the chrome just runs orb-less.
        let orb = fluor::host::icon::Icon::from_vsf_bytes(include_bytes!("../assets/opsin_orb.vsf")).ok();
        let viewport = Viewport::new(1280, 800);
        let mut hit_counter: HitId = HIT_NONE;
        let title = loaded.title().to_string();
        let chrome = DefaultChrome::new(viewport, title.clone(), orb, None, &mut hit_counter);
        let mut view = View::new(loaded, &mut hit_counter);
        view.set_source(dir_list.get(dir_idx).cloned());
        Self {
            title,
            chrome,
            view,
            view_w: 1280,
            view_h: 800,
            dir_list,
            dir_idx,
            hit_count: hit_counter,
            chord_lb_press: None,
            chord_lb_release: None,
            chord_rb_press: None,
            chord_rb_release: None,
            show_hitmask: false,
            debug_hit_colours: Vec::new(),
        }
    }

    /// Top bar height — the chrome strip, or nothing with the controls hidden.
    fn bar_h(&self, viewport: Viewport) -> f32 {
        if self.view.plain() { 0. } else { chrome::strip_height(viewport) }
    }

    /// The view's rectangle: everything below the bar.
    fn view_area(&self, viewport: Viewport) -> Area {
        let bar = self.bar_h(viewport);
        Area { x: 0., y: bar, w: viewport.width_px as f32, h: viewport.height_px as f32 - bar }
    }

    fn sync_area(&mut self, viewport: Viewport) {
        let area = self.view_area(viewport);
        self.view.set_area(area);
    }

    /// True iff both `[` and `]` are currently held (photon's rule): pressed more recently than released, or released within the grace window (X11 fires a synthetic Release for a held key the instant another key goes down).
    fn brackets_held(&self, now: Instant) -> bool {
        fn key_held(press: Option<Instant>, release: Option<Instant>, now: Instant) -> bool {
            match (press, release) {
                (Some(p), Some(r)) => p > r || now.duration_since(r) < CHORD_RELEASE_GRACE,
                (Some(_), None) => true,
                _ => false,
            }
        }
        key_held(self.chord_lb_press, self.chord_lb_release, now) && key_held(self.chord_rb_press, self.chord_rb_release, now)
    }

    /// `[]`-chord debug toggles — photon's letters, minus its vault/session ones: h hitmask, a alpha cycle, p skip-premult, f fps strip, w damage outline, d screen decay, b opaque-scan tint, c skip-chrome, l skip-controls, r force-redraw. Returns true if the letter fired.
    fn handle_chord_action(&mut self, ac: char, ctx: &mut Context) -> bool {
        use std::sync::atomic::Ordering;
        let mut acted = true;
        match ac {
            'h' => {
                self.show_hitmask = !self.show_hitmask;
                paint::DEBUG_SHOW_HITMASK.store(self.show_hitmask, Ordering::Relaxed);
                eprintln!("[]h hitmask = {}", self.show_hitmask);
                if self.show_hitmask {
                    // xorshift32 seeded from the clock's subsecond nanos (pure entropy, not a timestamp — no epoch involved) → 256 random opaque RGBs stored in α+darkness. Fresh palette every toggle so distinct IDs always pop visually.
                    let seed = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1)) | 1;
                    let mut s = seed;
                    self.debug_hit_colours.clear();
                    self.debug_hit_colours.reserve(256);
                    for _ in 0..256 {
                        let mut next = || {
                            s ^= s << 13;
                            s ^= s >> 17;
                            s ^= s << 5;
                            (s >> 16) & 0xFF
                        };
                        let (r, g, b) = (next(), next(), next());
                        let visible = (r << 16) | (g << 8) | b;
                        self.debug_hit_colours.push(0xFF000000 | (visible ^ 0x00FFFFFF));
                    }
                }
            }
            'a' => {
                // Cycle: off (0) → grayscale (1) → force-opaque (2) → off.
                let cur = paint::DEBUG_SHOW_ALPHA.load(Ordering::Relaxed);
                let next = (cur + 1) % 3;
                paint::DEBUG_SHOW_ALPHA.store(next, Ordering::Relaxed);
                let label = match next {
                    0 => "off",
                    1 => "grayscale",
                    _ => "force-opaque",
                };
                eprintln!("[]a show-alpha = {next} ({label})");
            }
            'p' => {
                let cur = paint::DEBUG_SKIP_PREMULT.load(Ordering::Relaxed);
                paint::DEBUG_SKIP_PREMULT.store(!cur, Ordering::Relaxed);
                eprintln!("[]p skip-premult = {}", !cur);
            }
            'f' => {
                let cur = paint::DEBUG_SHOW_FPS.load(Ordering::Relaxed);
                paint::DEBUG_SHOW_FPS.store(!cur, Ordering::Relaxed);
                eprintln!("[]f fps-strip = {}", !cur);
            }
            'w' => {
                let cur = paint::DEBUG_SHOW_DAMAGE.load(Ordering::Relaxed);
                paint::DEBUG_SHOW_DAMAGE.store(!cur, Ordering::Relaxed);
                eprintln!("[]w damage-outline = {}", !cur);
            }
            'd' => {
                let cur = paint::DEBUG_SHOW_FADE.load(Ordering::Relaxed);
                paint::DEBUG_SHOW_FADE.store(!cur, Ordering::Relaxed);
                eprintln!("[]d screen-decay = {}", !cur);
            }
            'b' => {
                let cur = paint::DEBUG_SHOW_OPAQUE_SCAN.load(Ordering::Relaxed);
                paint::DEBUG_SHOW_OPAQUE_SCAN.store(!cur, Ordering::Relaxed);
                eprintln!("[]b opaque-scan tint = {}", !cur);
            }
            'c' => {
                let cur = paint::DEBUG_SKIP_CHROME.load(Ordering::Relaxed);
                paint::DEBUG_SKIP_CHROME.store(!cur, Ordering::Relaxed);
                self.chrome.invalidate_chrome();
                eprintln!("[]c skip-chrome = {}", !cur);
            }
            'l' => {
                let cur = paint::DEBUG_SKIP_CONTROLS.load(Ordering::Relaxed);
                paint::DEBUG_SKIP_CONTROLS.store(!cur, Ordering::Relaxed);
                self.chrome.invalidate_chrome();
                eprintln!("[]l skip-controls = {}", !cur);
            }
            'r' => {
                self.chrome.invalidate_bg();
                self.chrome.invalidate_chrome();
                eprintln!("[]r force-redraw");
            }
            _ => acted = false,
        }
        if acted {
            ctx.window.request_redraw();
        }
        acted
    }

    /// Install a decoded frame from `path`: the view swaps it in, the title and source follow.
    fn install_path(&mut self, path: &Path, loaded: Loaded, ctx: &mut Context) {
        self.view.install(loaded, ctx);
        self.view.set_source(Some(path.to_path_buf()));
        self.title = self.view.title().to_string();
        self.chrome.set_title(&self.title);
    }

    /// Step `delta` (±1) through the folder, skipping images that fail to decode. Reloads the view and refits.
    fn navigate(&mut self, delta: isize, ctx: &mut Context) {
        let n = self.dir_list.len();
        if n <= 1 {
            return;
        }
        let mut idx = self.dir_idx;
        for _ in 0..n {
            idx = ((idx as isize + delta).rem_euclid(n as isize)) as usize;
            match load_image(&self.dir_list[idx]) {
                Ok(loaded) => {
                    self.dir_idx = idx;
                    let path = self.dir_list[idx].clone();
                    self.install_path(&path, loaded, ctx);
                    return;
                }
                Err(e) => eprintln!("opsin: {}: {e}", self.dir_list[idx].display()),
            }
        }
    }

    /// Open a path dropped onto the window: rebuild the folder list around it so arrow-nav works from there, then show it. Unsupported / undecodable drops are logged and ignored (current image stays).
    fn show_path(&mut self, path: &Path, ctx: &mut Context) {
        match load_image(path) {
            Ok(loaded) => {
                let (list, idx) = dir_list_for(path);
                self.dir_list = list;
                self.dir_idx = idx;
                self.install_path(path, loaded, ctx);
            }
            Err(e) => eprintln!("opsin: {}: {e}", path.display()),
        }
    }

    /// Plain mode moved the bar and the panel: the chrome's hit stamps are stale either way — mark the layer dirty so a returning chrome re-stamps, and wipe the map now so a hidden close button can't be clicked.
    fn after_plain_change(&mut self, ctx: &mut Context) {
        if self.view.take_plain_changed() {
            self.chrome.invalidate_chrome();
            self.chrome.hit_test_map.fill(HIT_NONE);
            self.sync_area(ctx.viewport);
            ctx.window.request_redraw();
        }
    }
}

impl Container for OpsinApp {
    fn visit(&mut self, f: &mut dyn FnMut(&mut dyn fluor::host::widget::Widget)) {
        self.chrome.visit(f);
        self.view.visit(f);
    }
}

impl FluorApp for OpsinApp {
    type UserEvent = Msg;

    /// The host's wake-sender arrives once before init: become the single instance now — bind the socket and ship the sender to the listener thread, which forwards each handed-over path to `on_user_event`. A clone goes to the view for its own background work (the target scan).
    fn set_event_proxy(&mut self, proxy: std::sync::Arc<dyn fluor::host::WakeSender<Self::UserEvent>>) {
        self.view.set_wake(proxy.clone());
        // macOS never spawns the second process the socket exists to catch — Launch Services routes a Finder open into THIS process. Same destination, so the same Msg; only the transport differs. This is the one moment the delegate method can be added: after winit built the EventLoop, before it runs.
        #[cfg(target_os = "macos")]
        {
            let proxy = proxy.clone();
            crate::mac_open::install(move |path| {
                let _ = proxy.send(Msg::Open(Some(path)));
            });
        }
        if let Some(sock) = crate::instance::listen(move |path| {
            let _ = proxy.send(Msg::Open(path));
        }) {
            if let Ok(mut cell) = SOCKET.lock() {
                *cell = Some(sock);
            }
        }
    }

    fn on_user_event(&mut self, event: Self::UserEvent, ctx: &mut Context) -> EventResponse {
        #[allow(unreachable_patterns)]
        match event {
            Msg::Open(path) => {
                if let Some(path) = path {
                    self.show_path(&path, ctx);
                }
                // Front + focus even if the load failed (the user just asked for this window) — WITHOUT moving it: the composition, panel, and window rect are the operator's and survive a handoff.
                EventResponse::Raise
            }
            other => {
                self.view.on_msg(other, ctx);
                EventResponse::Handled
            }
        }
    }

    fn title(&self) -> &str {
        &self.title
    }

    /// OS window icon (taskbar / alt-tab). The host applies this at window creation — Windows/X11 only; Wayland/macOS source the icon from packaging. Same orb the chrome draws.
    fn window_icon(&self) -> Option<&fluor::host::icon::Icon> {
        self.chrome.app_icon.as_ref()
    }

    fn init(&mut self, ctx: &mut Context) {
        self.chrome.resize(ctx.viewport);
        self.view_w = ctx.viewport.width_px as usize;
        self.view_h = ctx.viewport.height_px as usize;
        self.sync_area(ctx.viewport);
        // No fit here — init runs against the guessed pre-surface viewport; the view fits at its first render with real dims.
    }

    fn on_resize(&mut self, w: u32, h: u32, ctx: &mut Context) {
        self.chrome.resize(ctx.viewport);
        self.view_w = w as usize;
        self.view_h = h as usize;
        self.chrome.set_full_edge(ctx.is_maximized);
        self.sync_area(ctx.viewport);
        // Nothing view-related to do: the transform is span-relative and derived from the live area at render, so the composition rides the resize by construction.
    }

    fn on_zoom(&mut self, factor: f32, anchor_x: Coord, anchor_y: Coord, ctx: &mut Context) {
        // A pinch / Ctrl+wheel zooms the PICTURE about the anchor; the window's own scale is untouched.
        self.view.zoom_around(factor, anchor_x, anchor_y);
        ctx.window.request_redraw();
    }

    fn owns_zoom_gesture(&self) -> bool {
        true
    }

    fn on_event(&mut self, event: &FEvent, ctx: &mut Context) -> EventResponse {
        self.sync_area(ctx.viewport);
        match event {
            FEvent::CursorMoved { .. } => {
                // Use ctx.cursor_x/y (window-relative) — the event's own x/y are raw screen coords, offset by the window origin in the fullscreen-compositor model, so they'd desync everything else (chrome hit-test, image blit) which all work in window space.
                let hit = self.chrome.hit_at(ctx.cursor_x, ctx.cursor_y);
                let r = self.view.on_event(event, ctx, hit);
                self.after_plain_change(ctx);
                if r == EventResponse::Handled {
                    return r;
                }
                if self.chrome.set_hover(hit) {
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            FEvent::CursorLeft => {
                if self.chrome.set_hover(HIT_NONE) {
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            FEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left } => {
                let hit = self.chrome.hit_at(ctx.cursor_x, ctx.cursor_y);
                if self.chrome.owns_hit(hit) {
                    // Chrome button (close/min/max/orb) — dispatch thru the Container walk, same as panes.
                    let (x, y, mods) = (ctx.cursor_x, ctx.cursor_y, ctx.modifiers);
                    let mut response = EventResponse::Pass;
                    self.visit(&mut |w| {
                        if w.id() == hit {
                            if let Some(c) = w.click() {
                                response = c.on_click(x, y, mods);
                            }
                        }
                    });
                    return response;
                }
                let edge = chrome::get_resize_edge(ctx.viewport, ctx.cursor_x, ctx.cursor_y);
                if edge != ResizeEdge::None {
                    return EventResponse::StartResize(edge);
                }
                if ctx.cursor_y < self.bar_h(ctx.viewport) {
                    return EventResponse::StartWindowDrag;
                }
                let r = self.view.on_event(event, ctx, hit);
                self.after_plain_change(ctx);
                match r {
                    // Panel dead space and the backdrop (letterbox margin) move the window — the hand is already there when arranging the workspace.
                    EventResponse::Pass => EventResponse::StartWindowDrag,
                    other => other,
                }
            }
            FEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left } => {
                let hit = self.chrome.hit_at(ctx.cursor_x, ctx.cursor_y);
                self.view.on_event(event, ctx, hit);
                EventResponse::Pass
            }
            FEvent::MouseWheel { .. } => {
                let hit = self.chrome.hit_at(ctx.cursor_x, ctx.cursor_y);
                self.view.on_event(event, ctx, hit)
            }
            FEvent::Focused(focused) => {
                self.view.set_focused(*focused);
                // Forward to the chrome so the title colour + orb ring/darken track focus (the tint logic lives in DefaultChrome, driven by its `focused` flag — it just needs to be told).
                if self.chrome.set_focused(*focused) {
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            FEvent::DroppedFile(path) => {
                self.show_path(Path::new(path), ctx);
                EventResponse::Handled
            }
            FEvent::KeyboardInput { event: kev } => {
                // Bracket chord first, on BOTH press and release (photon's scheme) — the debug action must fire before normal key routing so a chord letter doesn't also trigger its app binding.
                if let Key::Character(c) = &kev.logical_key {
                    let cs = c.as_str();
                    let now = Instant::now();
                    let mut action_char: Option<char> = None;
                    match (cs, kev.state) {
                        ("[", ElementState::Pressed) => self.chord_lb_press = Some(now),
                        ("[", ElementState::Released) => self.chord_lb_release = Some(now),
                        ("]", ElementState::Pressed) => self.chord_rb_press = Some(now),
                        ("]", ElementState::Released) => self.chord_rb_release = Some(now),
                        (_, ElementState::Pressed) if !kev.repeat => {
                            if self.brackets_held(now) {
                                action_char = c.to_ascii_lowercase().chars().next();
                            }
                        }
                        _ => {}
                    }
                    if let Some(ac) = action_char {
                        if self.handle_chord_action(ac, ctx) {
                            return EventResponse::Handled;
                        }
                    }
                }
                if kev.state != ElementState::Pressed {
                    return EventResponse::Pass;
                }
                let ctrl = ctx.modifiers.control_key() || ctx.modifiers.super_key();
                match &kev.logical_key {
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.navigate(-1, ctx);
                        return EventResponse::Handled;
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        self.navigate(1, ctx);
                        return EventResponse::Handled;
                    }
                    // Ctrl+C / Ctrl+V: the IDT clipboard — copy the current frame's DSR magic-9, paste it into the current frame (same camera only).
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("c") => {
                        match self.dir_list.get(self.dir_idx).ok_or_else(|| "no image loaded".to_string()).and_then(|p| crate::idt::IdtClip::copy_from(p)).and_then(|clip| clip.save().map(|path| (clip, path))) {
                            Ok((clip, path)) => println!("opsin: copied IDT from {} → {}\n{}", clip.source, path.display(), clip.to_text()),
                            Err(e) => eprintln!("opsin: copy IDT failed: {e}"),
                        }
                        return EventResponse::Handled;
                    }
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("v") => {
                        let Some(target) = self.dir_list.get(self.dir_idx).cloned() else {
                            eprintln!("opsin: paste IDT: no image loaded");
                            return EventResponse::Handled;
                        };
                        // Shift = force: paste over a fingerprint mismatch (the lens-change case). The report line carries the WARNING.
                        let force = ctx.modifiers.shift_key();
                        match crate::idt::IdtClip::load().and_then(|clip| clip.paste_into(&target, force)) {
                            Ok(report) => {
                                println!("opsin: {report}");
                                // The file changed under us — reload so the display renders through the pasted IDT.
                                self.show_path(&target, ctx);
                            }
                            Err(e) => eprintln!("opsin: paste IDT failed: {e}"),
                        }
                        return EventResponse::Handled;
                    }
                    _ => {}
                }
                let r = self.view.on_event(event, ctx, HIT_NONE);
                self.after_plain_change(ctx);
                r
            }
            _ => EventResponse::Pass,
        }
    }

    fn render(&mut self, target: &mut [u32], ctx: &mut Context) {
        self.sync_area(ctx.viewport);
        let buf_w = ctx.viewport.width_px as usize;
        let buf_h = ctx.viewport.height_px as usize;
        let clip = Some(Clip::new(ctx.damage_clip.x0, ctx.damage_clip.y0, ctx.damage_clip.x1, ctx.damage_clip.y1));
        let plain = self.view.plain();

        // Front-to-back under-blend: perimeter hairline first (must own the window edge), then chrome controls, then the bar, then the view composes under those.
        self.chrome.rasterize_perimeter(target, buf_w, buf_h, ctx.clip_mask);
        if !plain {
            self.chrome.rasterize_chrome(ctx.damage, ctx.text, ctx.clip_mask);
            self.chrome.flatten_into(target, buf_w, buf_h, clip);
        }

        // ── Top bar ── full-width, Photon's horizontal-streak noise texture (composes UNDER the already-flattened chrome, so orb/title/controls stay on top). Base toned to the controls-strip grey so the textured area and the flat control fill read as one bar. The bar is the canonical window-move handle (and future menu home).
        let bar_h = self.bar_h(ctx.viewport) as usize;
        let mut canvas = Canvas::new(target, buf_w, buf_h, ctx.damage);
        if !plain {
            let bar_clip = Clip::new(0, 0, buf_w, bar_h.min(buf_h));
            paint::background_noise(&mut canvas, 0, true, 0, Some(bar_clip), Some(BAR_TEXTURE_BASE));
            paint::fill_rect(&mut canvas, 0, bar_h as isize, buf_w as isize, 0, HAIRLINE, clip, None);
        }

        // The view: image area + panel + HUD + backdrop, below the bar.
        self.view.render(&mut canvas, ctx.viewport, ctx.text, ctx.damage_clip, (ctx.cursor_x, ctx.cursor_y), &mut self.chrome.hit_test_map);

        // Hit-mask overlay ([]h) — every pixel replaced by its hit id's palette colour, drawn LAST so it shows exactly what hit_at returns. `.get` justified: the map can hold stale stamps at ids past the 256-colour palette; unknown ids render transparent instead of panicking a debug view.
        if self.show_hitmask && !self.debug_hit_colours.is_empty() {
            let map = &self.chrome.hit_test_map;
            let n = map.len().min(target.len());
            for i in 0..n {
                target[i] = self.debug_hit_colours.get(map[i] as usize).copied().unwrap_or(0);
            }
        }
    }

    fn hit_test_map(&self) -> Option<(&[fluor::paint::HitId], usize, usize)> {
        Some((&self.chrome.hit_test_map, self.view_w, self.view_h))
    }

    /// The table the host actually PAINTS hover from — set_hovered alone is state; this pipe is what puts the tint on screen. One Container walk: chrome buttons + the view's pills each contribute their tint at their id's slot.
    fn overlay_deltas(&mut self) -> Vec<u32> {
        let count = self.hit_count as usize + 1;
        fluor::host::widget::build_overlay_deltas(self, count)
    }

    /// Parallel bbox table so the host bounds each tint scan to the widget's rect instead of the whole window.
    fn overlay_bboxes(&mut self, viewport_w: usize, viewport_h: usize) -> Vec<Option<fluor::canvas::PixelRect>> {
        let count = self.hit_count as usize + 1;
        fluor::host::widget::build_overlay_bboxes(self, count, viewport_w, viewport_h)
    }

    fn cursor_for(&self, x: Coord, y: Coord, ctx: &Context) -> CursorIcon {
        let hit = self.chrome.hit_at(x, y);
        if let Some(c) = self.view.cursor_for(x, y, hit) {
            return c;
        }
        // Chrome buttons → hand, like panes.
        if self.chrome.owns_hit(hit) && hit != self.chrome.app_icon_btn.id() {
            return CursorIcon::Pointer;
        }
        // Resize arrows only where a press would actually resize — the bar body below the sliver stays Default (it moves the window).
        match chrome::get_resize_edge(ctx.viewport, x, y) {
            ResizeEdge::Top | ResizeEdge::Bottom => CursorIcon::NsResize,
            ResizeEdge::Left | ResizeEdge::Right => CursorIcon::EwResize,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorIcon::NwseResize,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorIcon::NeswResize,
            ResizeEdge::None => CursorIcon::Default,
        }
    }
}
