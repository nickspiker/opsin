//! Live: a V4L2 camera as opsin's frame source, behind the `live` feature. What `chameleon live` does, hosted by the viewer instead of a terminal + egui window: the capture thread pulls MJPEG frames from the camera, decodes, turns them 180° (the mount), linearizes (the camera's gamma-2), applies the live colour matrix (3×3 with a per-row gain, from the panel's sliders), and emits two things per frame — the corrected stream to ffmpeg → a v4l2loopback device (`/dev/video10`, tagged BT.2020 gamma 2.2) for the call app, and the full-resolution linear frame to the UI thread (gated so a slow UI drops frames rather than queueing them), which lands in the viewer exactly like a decoded file: histogram, chart, EV, clip, HDR, HUD, and `E` (save a JPEG) all work on it. No loopback device ⇒ viewfinder only, and the HUD says so.
//!
//! With `calibrate` too, `C` requests a scan of the next frame: chameleon solves the matrix from a held-up target, the sliders jump to it, and the solved overlay is composited into both outputs for a few seconds so the fit is visible on the call. The matrix persists to `~/.config/Verichrome/live_colour.vsf` in chameleon's own section format, so the two tools share it.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const WIDTH: usize = 3840;
pub const HEIGHT: usize = 2160;
pub const LOOPBACK: &str = "/dev/video10";

/// The live colour matrix as the sliders hold it: three output rows of [r, g, b, gain]; the applied matrix is row × gain.
pub type Sliders = [[f32; 4]; 3];
pub const IDENTITY: Sliders = [[1., 0., 0., 1.], [0., 1., 0., 1.], [0., 0., 1., 1.]];

/// The starting matrix when none is saved: chameleon's own default with the `calibrate` feature (the same one `chameleon live` starts from), identity without it.
pub fn default_matrix() -> Sliders {
    #[cfg(feature = "calibrate")]
    {
        chameleon::DEFAULT_MATRIX
    }
    #[cfg(not(feature = "calibrate"))]
    {
        IDENTITY
    }
}

/// State shared between the UI and the capture thread.
pub struct Shared {
    pub matrix: Mutex<Sliders>,
    pub stop: AtomicBool,
    /// UI has consumed the last viewfinder frame — the thread only sends another when this is clear, so a slow UI drops frames instead of queueing them.
    pub frame_pending: AtomicBool,
    /// Viewfinder wanted at all. Off while opsin isn't the focused window (you're in the call app): the stream keeps going at its own cost and the UI does nothing — measured, the preview roughly doubles the CPU bill. Headless `--live` never sets it.
    pub viewfinder: AtomicBool,
    /// Mirrors of the viewer's encode-boundary state, applied to the loopback stream identically so the viewfinder IS the call feed: the clip indicator (chameleon's highlight/shadow clip), the EV gain (f32 bits, 2^ev — a global gain after the matrix, on top of the sliders' row gains), and the HDR rail.
    pub clip: AtomicBool,
    pub ev_gain: AtomicU32,
    pub hdr: AtomicBool,
    /// `calibrate`: scan the next frame.
    pub scan_requested: AtomicBool,
    /// A/V alignment. `pipe_ms`: opsin's measured share — V4L2 capture timestamp → the frame handed to ffmpeg, EMA over frames (ms ×10 for a decimal). `extra_ms`: the parts we can't measure, set by the operator — the Sigma's sensor→USB delay plus ffmpeg→loopback→call-app capture; persisted. The aligned mic is delayed by the sum.
    pub pipe_ms10: AtomicU32,
    pub extra_ms: AtomicU32,
    /// Recording: the UI sets a path to start and clears it to stop; the thread owns the encoder. `rec_frames` counts frames written (the HUD's clock).
    pub record: Mutex<Option<PathBuf>>,
    pub rec_frames: AtomicU32,
    /// Mic level meter: peak of the last 20 ms block, 0..1 as f32 bits; 0 while nothing is being read.
    pub mic_peak: AtomicU32,
    /// The last two seconds of the mic (s16 mono 48 kHz) with the wall time (ms since the epoch) of its newest sample — the recorder's audio source, so a recording can start from the sample that matches its first frame.
    pub mic_ring: Mutex<(std::collections::VecDeque<i16>, f64)>,
    /// A recording asks the mic thread for audio: (FIFO path, wall ms of the first sample wanted). `None` ends the feed (EOF to the encoder).
    pub audio_feed: Mutex<Option<(PathBuf, f64)>>,
    /// Frames the solved overlay stays composited for (counts down).
    pub overlay_frames: AtomicU32,
}

impl Shared {
    pub fn new(matrix: Sliders) -> Arc<Shared> {
        Arc::new(Shared { matrix: Mutex::new(matrix), stop: AtomicBool::new(false), frame_pending: AtomicBool::new(false), viewfinder: AtomicBool::new(true), clip: AtomicBool::new(false), ev_gain: AtomicU32::new(1f32.to_bits()), hdr: AtomicBool::new(false), scan_requested: AtomicBool::new(false), overlay_frames: AtomicU32::new(0), pipe_ms10: AtomicU32::new(0), extra_ms: AtomicU32::new(load_extra_ms()), record: Mutex::new(None), rec_frames: AtomicU32::new(0), mic_peak: AtomicU32::new(0), mic_ring: Mutex::new((std::collections::VecDeque::new(), 0.)), audio_feed: Mutex::new(None) })
    }
    /// Total audio delay to apply, in ms: measured pipe + operator's constant.
    pub fn av_delay_ms(&self) -> f32 {
        self.pipe_ms10.load(Ordering::Relaxed) as f32 / 10. + self.extra_ms.load(Ordering::Relaxed) as f32
    }
    pub fn applied(&self) -> [f32; 9] {
        let s = *self.matrix.lock().unwrap();
        let mut m = [0f32; 9];
        for r in 0..3 {
            for c in 0..3 {
                m[r * 3 + c] = s[r][c] * s[r][3];
            }
        }
        m
    }
}

/// One viewfinder frame for the UI at full camera resolution: signed linear Rec.2020 (white = 65535) after the matrix — the viewer's `lin` contract — plus the camera's own 8-bit codes for the histogram.
pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub lin: Vec<i32>,
    pub codes: Vec<u16>,
}

/// What a live scan produced (calibrate): the HUD readout and chameleon's report.
pub struct ScanReport {
    pub readout: String,
    pub report: String,
}

pub enum LiveMsg {
    Frame(Frame),
    Status(String),
    /// Meter tick — a repaint request while no frames are flowing to the UI (unfocused), so the meter still moves.
    Level,
    #[cfg(feature = "calibrate")]
    Scan(Result<ScanReport, String>),
}

/// 2×2 box-bin an RGBA f32 raster (chameleon's `bin_rectangular` for 4 channels, bins = 2).
#[cfg(feature = "calibrate")]
fn bin2_rgba(src: &[f32], w: usize, h: usize) -> Vec<f32> {
    let (bw, bh) = (w / 2, h / 2);
    let mut out = vec![0f32; bw * bh * 4];
    for y in 0..bh {
        for x in 0..bw {
            for c in 0..4 {
                let s = |dx: usize, dy: usize| src[((y * 2 + dy) * w + x * 2 + dx) * 4 + c];
                out[(y * bw + x) * 4 + c] = (s(0, 0) + s(1, 0) + s(0, 1) + s(1, 1)) * 0.25;
            }
        }
    }
    out
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("Verichrome").join("live_colour.vsf")
}

/// chameleon's `live_colour.vsf`: one section `colour_matrix`, field `matrix` = a 3×3 f32 tensor with the gains folded in.
pub fn load_matrix() -> Option<Sliders> {
    let data = std::fs::read(config_path()).ok()?;
    let mut ptr = 0;
    let section = vsf::VsfSection::parse(&data, &mut ptr).ok()?;
    let field = section.get_field("matrix")?;
    if let Some(vsf::VsfType::t_f5(t)) = field.values.first() {
        if t.data.len() == 9 {
            let d = &t.data;
            return Some([[d[0], d[1], d[2], 1.], [d[3], d[4], d[5], 1.], [d[6], d[7], d[8], 1.]]);
        }
    }
    None
}

/// The part of the operator's constant that is downstream of opsin (ffmpeg → loopback → the call app's capture) and so does NOT apply to a recorded file: the file's video is opsin's own output. What's left of the constant is the camera's sensor→USB time, which does.
pub const DOWNSTREAM_MS: u32 = 60;

/// Default for the unmeasurable part of the A/V delay: a UVC camera's sensor→USB (~1–2 frames at 15 fps) plus ffmpeg→loopback→app capture. Tune by ear with , and . — a clap on the call is the calibration.
pub const DEFAULT_EXTRA_MS: u32 = 150;

/// The operator's A/V constant, from `live_colour.vsf` (field `av_extra_ms`, which chameleon's reader ignores), else the default.
pub fn load_extra_ms() -> u32 {
    let Ok(data) = std::fs::read(config_path()) else { return DEFAULT_EXTRA_MS };
    let mut ptr = 0;
    let Ok(section) = vsf::VsfSection::parse(&data, &mut ptr) else { return DEFAULT_EXTRA_MS };
    match section.get_field("av_extra_ms").and_then(|f| f.values.first()) {
        Some(vsf::VsfType::u5(v)) => *v,
        _ => DEFAULT_EXTRA_MS,
    }
}

pub fn save_matrix(s: &Sliders) -> Result<PathBuf, String> {
    save_matrix_and_extra(s, load_extra_ms())
}

pub fn save_matrix_and_extra(s: &Sliders, extra_ms: u32) -> Result<PathBuf, String> {
    let mut flat = Vec::with_capacity(9);
    for r in 0..3 {
        for c in 0..3 {
            flat.push(s[r][c] * s[r][3]);
        }
    }
    let mut section = vsf::VsfSection::new("colour_matrix");
    section.add_field("matrix", vsf::VsfType::t_f5(vsf::Tensor::new(vec![3, 3], flat)));
    section.add_field("av_extra_ms", vsf::VsfType::u5(extra_ms));
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, section.encode()).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Wall clock in milliseconds since the epoch — the clock the recorder aligns audio and video on.
fn wall_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1e3).unwrap_or(0.)
}

/// CLOCK_MONOTONIC in milliseconds — the clock V4L2 stamps UVC buffers with, so a capture timestamp and "now" are comparable. libc-free.
fn monotonic_ms() -> f64 {
    #[repr(C)]
    struct Timespec {
        sec: i64,
        nsec: i64,
    }
    unsafe extern "C" {
        fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
    }
    let mut ts = Timespec { sec: 0, nsec: 0 };
    unsafe {
        clock_gettime(1, &mut ts);
    }
    ts.sec as f64 * 1e3 + ts.nsec as f64 / 1e6
}

/// The aligned microphone: a `pipewire` process hosting a filter-chain (the default mic → a delay line → a virtual source named "opsin aligned mic"), the delay set at runtime with `pw-cli`. Dropping it ends the process and the source with it. Absent PipeWire ⇒ `None`, and the video runs without it.
pub struct AlignedMic {
    child: std::process::Child,
    node_id: Option<u32>,
    last_set_ms: f32,
}

impl AlignedMic {
    pub fn start(delay_ms: f32) -> Option<AlignedMic> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let conf = dir.join("opsin-mic.conf");
        let text = format!(
            r#"context.properties = {{ log.level = 0 }}
context.spa-libs = {{ audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }}
context.modules = [
    {{ name = libpipewire-module-rt args = {{ nice.level = -11 }} flags = [ ifexists nofail ] }}
    {{ name = libpipewire-module-protocol-native }}
    {{ name = libpipewire-module-client-node }}
    {{ name = libpipewire-module-adapter }}
    {{ name = libpipewire-module-filter-chain
        args = {{
            node.description = "opsin aligned mic"
            media.name       = "opsin aligned mic"
            filter.graph = {{ nodes = [ {{ type = builtin name = delay label = delay config = {{ "max-delay" = 2.0 }} control = {{ "Delay (s)" = {:.3} }} }} ] }}
            capture.props  = {{ node.name = "opsin_mic_capture" node.passive = true audio.position = [ MONO ] }}
            playback.props = {{ node.name = "opsin_mic" media.class = Audio/Source audio.position = [ MONO ] }}
        }}
    }}
]
"#,
            delay_ms / 1000.
        );
        std::fs::write(&conf, text).ok()?;
        let child = std::process::Command::new("pipewire").arg("-c").arg(&conf).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().ok()?;
        let mut mic = AlignedMic { child, node_id: None, last_set_ms: delay_ms };
        // The node appears a moment after launch; find it by name (pw-dump is JSON — a line scan for our node.name then the nearest preceding "id" is enough here).
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Some(id) = Self::find_node() {
                mic.node_id = Some(id);
                break;
            }
        }
        Some(mic)
    }

    fn find_node() -> Option<u32> {
        let out = std::process::Command::new("pw-dump").output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let at = text.find("\"node.name\": \"opsin_mic_capture\"")?;
        let head = &text[..at];
        let id_at = head.rfind("\"id\": ")?;
        head[id_at + 6..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
    }

    /// Push a new delay when it moved by more than 5 ms since the last push.
    pub fn set_delay(&mut self, delay_ms: f32) {
        if (delay_ms - self.last_set_ms).abs() < 5. {
            return;
        }
        let Some(id) = self.node_id else { return };
        let _ = std::process::Command::new("pw-cli").args(["set-param", &id.to_string(), "Props", &format!("{{ params = [ \"delay:Delay (s)\" {:.3} ] }}", delay_ms / 1000.)]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        self.last_set_ms = delay_ms;
    }

    pub fn active(&self) -> bool {
        self.node_id.is_some()
    }
}

impl Drop for AlignedMic {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The recorder's encode settings (after the two inputs and the codec): H.264 8-bit, x264 ultrafast — measured on real 4K frames from this camera: ultrafast 27 fps, superfast 20, ProRes LT 22, DNxHR 11, x265 ~5; only ultrafast has real headroom over 15 while the stream runs beside it (the synthetic pattern it was first timed on compresses trivially). No -tune zerolatency: it turns off frame threading, which at 4K is the speed. BT.2020-tagged like the stream, PCM audio. QuickTime and Resolve Studio both decode it.
const REC_ENCODE: &[&str] = &[
    "-preset", "ultrafast", "-crf", "17", "-pix_fmt", "yuv420p",
    "-color_primaries", "bt2020", "-color_trc", "bt709", "-colorspace", "bt2020nc",
    "-c:a", "pcm_s16le", "-ar", "48000",
];

/// A recording in progress: ffmpeg encoding the corrected frames (H.264 8-bit 4:2:0 at the camera's native 15 fps, see REC_ENCODE) with the aligned mic as PCM and a time-of-day timecode track from the clock at start, into a MOV, BT.2020-tagged like the stream.
pub struct Recorder {
    child: std::process::Child,
    /// Frames go to a writer thread thru a bounded channel, so the encoder's back-pressure can never stall the capture loop (and the loopback with it). A full queue drops the frame — with wall-clock stamps that's a duplicated frame in the file, not drift.
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<()>>,
    pub path: PathBuf,
    pub dropped: Arc<AtomicU32>,
}

impl Recorder {
    pub fn start(path: &std::path::Path, audio_fifo: &std::path::Path) -> Result<Recorder, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // Time-of-day timecode at the camera's rate: HH:MM:SS:FF, frames from the sub-second part.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?;
        let (h, m, sec, ff) = local_hms(now.as_secs(), (now.subsec_millis() as u32 * 15 / 1000).min(14));
        let timecode = format!("{h:02}:{m:02}:{sec:02}:{ff:02}");
        let size = format!("{WIDTH}x{HEIGHT}");
        // A fresh FIFO for the audio: opsin's mic thread writes s16 mono into it from the sample that matches the first frame. Both inputs are plain count-based streams that start at 0 together — opsin owns the clock (CFR-conformed video, contiguous audio), ffmpeg does no timestamp arithmetic at all.
        let _ = std::fs::remove_file(audio_fifo);
        let status = std::process::Command::new("mkfifo").arg(audio_fifo).status().map_err(|e| format!("mkfifo: {e}"))?;
        if !status.success() {
            return Err("mkfifo failed".to_string());
        }
        let mut child = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel", "info", "-stats",
                "-thread_queue_size", "64", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &size, "-framerate", "15", "-i", "pipe:0",
                "-thread_queue_size", "1024", "-f", "s16le", "-ar", "48000", "-ac", "1", "-i",
            ])
            .arg(audio_fifo)
            .args(["-map", "0:v", "-map", "1:a", "-c:v", "libx264"])
            .args(REC_ENCODE)
            .args(["-timecode", &timecode, "-metadata", "encoder=opsin live", "-movflags", "+faststart", "-y"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| format!("ffmpeg: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("ffmpeg: no stdin")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
        let writer = std::thread::Builder::new()
            .name("opsin-rec-writer".into())
            .spawn(move || {
                while let Ok(frame) = rx.recv() {
                    if stdin.write_all(&frame).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Recorder { child, tx: Some(tx), writer: Some(writer), path: path.to_path_buf(), dropped: Arc::new(AtomicU32::new(0)) })
    }
    /// Queue one rgb24 frame for the writer. `false` once the writer is gone (encoder closed) — the recording is over; a full queue just drops this frame and counts it.
    pub fn write(&mut self, frame: &[u8]) -> bool {
        let Some(tx) = self.tx.as_ref() else { return false };
        match tx.try_send(frame.to_vec()) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        }
    }
    /// End the recording: EOF on the video pipe (the mic thread EOFs the audio FIFO when its feed is cleared) and ffmpeg finishes the file on its own. A SIGINT after a grace period covers a FIFO that never got a reader.
    pub fn finish(mut self) {
        drop(self.tx.take());
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        // Drain: the writer queued at most a handful of frames, so this is short — but leave room for a slow flush.
        for _ in 0..300 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe {
            kill(self.child.id() as i32, 2);
        }
        let _ = self.child.wait();
    }
}

/// Unix seconds → local (h, m, s) plus the frame number given, via `localtime_r` — libc-free declaration, no chrono.
fn local_hms(secs: u64, frame: u32) -> (u32, u32, u32, u32) {
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
        gmtoff: i64,
        zone: *const u8,
    }
    unsafe extern "C" {
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let t = secs as i64;
    let mut tm = Tm { sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0, wday: 0, yday: 0, isdst: 0, gmtoff: 0, zone: std::ptr::null() };
    unsafe {
        localtime_r(&t, &mut tm);
    }
    (tm.hour as u32, tm.min as u32, tm.sec as u32, frame)
}

/// Local date-time for a file name: `YYYYmmdd-HHMMSS`.
pub fn local_stamp() -> String {
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
        gmtoff: i64,
        zone: *const u8,
    }
    unsafe extern "C" {
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let mut tm = Tm { sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0, wday: 0, yday: 0, isdst: 0, gmtoff: 0, zone: std::ptr::null() };
    unsafe {
        localtime_r(&t, &mut tm);
    }
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", tm.year + 1900, tm.mon + 1, tm.mday, tm.hour, tm.min, tm.sec)
}

/// The mic thread: `pw-record` streams the DEFAULT source (the raw mic — what the aligned mic delays) as s16 mono 48 kHz in 20 ms blocks. Each block updates the meter peak and the 2 s ring; while a recording has asked for audio, blocks also go to its FIFO — starting with the ring's backlog from the wall time it named, so the audio lines up with the first frame. Ends with the stream.
fn start_mic_meter(shared: Arc<Shared>, source: &str, send: Arc<dyn Fn(LiveMsg) + Send + Sync>) -> Option<std::thread::JoinHandle<()>> {
    let mut child = std::process::Command::new("pw-record")
        .args(["--target", source, "--format", "s16", "--rate", "48000", "--channels", "1", "-"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    std::thread::Builder::new()
        .name("opsin-mic-meter".into())
        .spawn(move || {
            use std::io::Read;
            const RATE: usize = 48000;
            const BLOCK: usize = RATE / 50; // 20 ms
            let mut buf = vec![0u8; BLOCK * 2];
            let mut ticks = 0u32;
            let mut feed: Option<(PathBuf, std::fs::File)> = None;
            while !shared.stop.load(Ordering::Relaxed) {
                if out.read_exact(&mut buf).is_err() {
                    break;
                }
                let now_ms = wall_ms();
                let samples: Vec<i16> = buf.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
                let peak = samples.iter().map(|v| (*v as i32).unsigned_abs()).max().unwrap_or(0) as f32 / 32768.;
                shared.mic_peak.store(peak.to_bits(), Ordering::Relaxed);
                {
                    let mut ring = shared.mic_ring.lock().unwrap();
                    ring.0.extend(samples.iter().copied());
                    while ring.0.len() > RATE * 2 {
                        ring.0.pop_front();
                    }
                    ring.1 = now_ms;
                }
                // Recording audio: open the FIFO when asked (ffmpeg has to be at the other end — retry until it is), send the backlog from the requested moment, then every block as it comes; EOF when the request is cleared.
                let want = shared.audio_feed.lock().unwrap().clone();
                match (&want, &mut feed) {
                    (Some((path, from_ms)), None) => {
                        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
                            let backlog: Vec<i16> = {
                                let ring = shared.mic_ring.lock().unwrap();
                                let n = (((ring.1 - from_ms) / 1000. * RATE as f64).round().max(0.) as usize).min(ring.0.len());
                                ring.0.iter().skip(ring.0.len() - n).copied().collect()
                            };
                            let mut f = f;
                            let bytes: Vec<u8> = backlog.iter().flat_map(|v| v.to_le_bytes()).collect();
                            if f.write_all(&bytes).is_ok() {
                                feed = Some((path.clone(), f));
                            }
                        }
                    }
                    (Some(_), Some((_, f))) => {
                        if f.write_all(&buf).is_err() {
                            feed = None;
                        }
                    }
                    (None, Some(_)) => {
                        feed = None; // drop closes the FIFO: EOF to the encoder
                    }
                    (None, None) => {}
                }
                // Frames drive repaints while the viewfinder runs; otherwise tick the UI at 5 Hz so the meter keeps moving.
                if !shared.viewfinder.load(Ordering::Relaxed) {
                    ticks += 1;
                    if ticks % 10 == 0 {
                        send(LiveMsg::Level);
                    }
                }
            }
            drop(feed);
            let _ = child.kill();
            let _ = child.wait();
            shared.mic_peak.store(0, Ordering::Relaxed);
        })
        .ok()
}

/// The first MJPEG capture device, preferring one already at 3840×2160 (chameleon's rule); loopback devices are skipped.
pub fn find_device() -> Result<String, String> {
    use v4l::video::Capture;
    let mut paths: Vec<String> = std::fs::read_dir("/dev").map_err(|e| e.to_string())?.flatten().map(|e| e.path().to_string_lossy().into_owned()).filter(|p| p.starts_with("/dev/video")).collect();
    paths.sort();
    let mut fallback = None;
    for p in &paths {
        if p == LOOPBACK || p == "/dev/video11" {
            continue;
        }
        let Ok(dev) = v4l::Device::with_path(p) else { continue };
        let Ok(f) = Capture::format(&dev) else { continue };
        if f.fourcc == v4l::FourCC::new(b"MJPG") {
            if f.width as usize == WIDTH && f.height as usize == HEIGHT {
                return Ok(p.clone());
            }
            fallback.get_or_insert(p.clone());
        }
    }
    fallback.ok_or_else(|| "no MJPEG camera found under /dev/video*".to_string())
}

/// Start the capture thread. Returns once the device is open (errors here are immediate: no camera, wrong format); everything after is reported thru `send`.
pub fn start(shared: Arc<Shared>, send: impl Fn(LiveMsg) + Send + Sync + 'static) -> Result<std::thread::JoinHandle<()>, String> {
    let send: Arc<dyn Fn(LiveMsg) + Send + Sync> = Arc::new(send);
    use v4l::io::traits::CaptureStream;
    use v4l::video::Capture;
    let path = find_device()?;
    let mut dev = v4l::Device::with_path(&path).map_err(|e| format!("{path}: {e} (another process has the camera? `fuser {path}`)"))?;
    // Close-on-exec on the camera fd: every child we spawn (ffmpeg, pipewire, pw-record) would otherwise inherit it and keep the camera busy after we're gone — an orphaned pipewire from a killed session did exactly that (2026-09-17).
    {
        unsafe extern "C" {
            fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
        }
        unsafe {
            fcntl(dev.handle().fd(), 2, 1); // F_SETFD, FD_CLOEXEC
        }
    }
    let mut fmt = Capture::format(&dev).map_err(|e| e.to_string())?;
    fmt.width = WIDTH as u32;
    fmt.height = HEIGHT as u32;
    fmt.fourcc = v4l::FourCC::new(b"MJPG");
    let fmt = Capture::set_format(&dev, &fmt).map_err(|e| e.to_string())?;
    if fmt.width as usize != WIDTH || fmt.height as usize != HEIGHT || fmt.fourcc != v4l::FourCC::new(b"MJPG") {
        return Err(format!("{path}: wanted {WIDTH}×{HEIGHT} MJPG, got {}×{} {}", fmt.width, fmt.height, String::from_utf8_lossy(&fmt.fourcc.repr)));
    }
    // Two buffers: the driver fills one while we process the other. One (chameleon's choice) drops every frame whose successor arrives mid-processing — measured ~10 of 15 fps kept with the recorder running.
    let mut stream = v4l::io::mmap::Stream::with_buffers(&mut dev, v4l::buffer::Type::VideoCapture, 2).map_err(|e| e.to_string())?;

    // ffmpeg → loopback, exactly chameleon's invocation. Absent loopback ⇒ viewfinder only.
    let mut ffmpeg = if std::path::Path::new(LOOPBACK).exists() {
        let size = format!("{WIDTH}x{HEIGHT}");
        std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-fflags", "nobuffer", "-flags", "low_delay", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &size, "-framerate", "15", "-color_range", "pc", "-colorspace", "bt2020nc", "-color_primaries", "bt2020", "-color_trc", "gamma22", "-i", "pipe:0", "-f", "v4l2", "-pix_fmt", "yuv420p", "-colorspace", "bt2020nc", "-color_primaries", "bt2020", "-color_trc", "gamma22", "-color_range", "pc", "-vsync", "0", LOOPBACK])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .ok()
    } else {
        None
    };
    let status = match &ffmpeg {
        Some(_) => format!("live: {path} → {LOOPBACK}"),
        None => format!("live: {path} (no {LOOPBACK} — viewfinder only; load v4l2loopback for a virtual camera)"),
    };
    send(LiveMsg::Status(status));

    std::thread::Builder::new()
        .name("opsin-live".into())
        .spawn(move || {
            // LUTs: camera code → linear u16 (gamma 2), and linear → loopback byte (gamma 1/2.4), as chameleon builds them.
            let lin_lut: Vec<u16> = (0..256u32).map(|v| ((v as f32 / 255.).powi(2) * 65535.).round() as u16).collect();
            let enc_lut: Vec<u8> = (0..65536u32).map(|v| ((v as f32 / 65535.).powf(1. / 2.4) * 255.).min(255.) as u8).collect();
            let n = WIDTH * HEIGHT;
            let mut out = vec![0u8; n * 3];
            let mut lin_buf: Vec<i32> = Vec::new();
            let mut codes_buf: Vec<u16> = Vec::new();
            let mut stdin = ffmpeg.as_mut().and_then(|f| f.stdin.take());
            // (x0, y0, w, h, rgba) in un-rotated frame coordinates, display-linear white=1.
            let mut overlay: Option<(usize, usize, usize, usize, Vec<f32>)> = None;
            #[cfg(feature = "calibrate")]
            let mut cal_settings: Option<chameleon::CurrentSettings> = None;
            #[cfg(feature = "calibrate")]
            let mut cal_info = chameleon::RawInfo {
                width: WIDTH, height: HEIGHT, rgb: true, bitdepth: 8, bitdepthold: 8, black: 0., white: 1., make: "Webcam".into(), model: "Live Capture".into(),
                makeoffset: 0, makelen: 0, modeloffset: 0, modellen: 0, cfa: Vec::new(), cfaw: 0, cfah: 0, blackoffset: 0, blackcount: 0, blacktype: 0, orientation: 9, compression: false,
                cam2terminal9: [-0.5, 0., 3.5, -0.25, 5., -1.5, 1.5, -0.5, 0.], magic9inv: [0; 72], magicoffset: 0, profileoffset: 0, curveoffset: 0, imagedataoffset: 0, ifdoffset: 0, duck: false, save_scan: false,
            };
            let mut mic = AlignedMic::start(shared.av_delay_ms());
            send(LiveMsg::Status(match &mic {
                Some(m) if m.active() => format!("live: {path} → {LOOPBACK}; mic → \"opsin aligned mic\" (delayed to match)"),
                Some(_) => format!("live: {path} → {LOOPBACK}; aligned mic started but not found in the graph"),
                None => format!("live: {path} → {LOOPBACK}; no PipeWire — no aligned mic"),
            }));
            // The mic thread reads the RAW default source: the meter shows the same level the aligned mic carries (it is that signal, delayed), and a recording wants the undelayed samples to place by wall time itself.
            let _meter = start_mic_meter(shared.clone(), "@DEFAULT_SOURCE@", send.clone());
            let mut recorder: Option<Recorder> = None;
            // Recording clock: the wall time the next output frame is due; frames are duplicated or dropped to hold exactly 15 fps, so the file's frame index IS time.
            let mut rec_next_due = 0f64;
            const FRAME_MS: f64 = 1000. / 15.;
            let fifo = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("opsin-rec-audio.fifo");
            let mut frame_no = 0u32;
            while !shared.stop.load(Ordering::Relaxed) {
                // Recording requests from the UI: a new path starts (or replaces) a recording, None ends it.
                {
                    let want = shared.record.lock().unwrap().clone();
                    match (want, recorder.as_ref().map(|r| r.path.clone())) {
                        (Some(p), cur) if cur.as_ref() != Some(&p) => {
                            if let Some(r) = recorder.take() {
                                r.finish();
                            }
                            match Recorder::start(&p, &fifo) {
                                Ok(r) => {
                                    shared.rec_frames.store(0, Ordering::Relaxed);
                                    // The first frame is due now; its audio starts at the moment that frame was captured: now − opsin's pipe − the camera's own latency (the operator's constant less the downstream part, which doesn't apply to a file).
                                    let now = wall_ms();
                                    rec_next_due = now;
                                    let cam_ms = shared.extra_ms.load(Ordering::Relaxed).saturating_sub(DOWNSTREAM_MS) as f64;
                                    let pipe_ms = shared.pipe_ms10.load(Ordering::Relaxed) as f64 / 10.;
                                    *shared.audio_feed.lock().unwrap() = Some((fifo.clone(), now - pipe_ms - cam_ms));
                                    send(LiveMsg::Status(format!("live: recording → {}", p.display())));
                                    recorder = Some(r);
                                }
                                Err(e) => {
                                    *shared.record.lock().unwrap() = None;
                                    send(LiveMsg::Status(format!("live: record failed: {e}")));
                                }
                            }
                        }
                        (None, Some(_)) => {
                            if let Some(r) = recorder.take() {
                                let p = r.path.clone();
                                let dropped = r.dropped.load(Ordering::Relaxed);
                                *shared.audio_feed.lock().unwrap() = None;
                                r.finish();
                                send(LiveMsg::Status(if dropped > 0 { format!("live: recording saved → {} ({dropped} frames dropped: encoder fell behind — duplicated in the file, timing intact)", p.display()) } else { format!("live: recording saved → {}", p.display()) }));
                            }
                        }
                        _ => {}
                    }
                }
                let Ok((mjpeg, meta)) = stream.next() else { continue };
                let captured_ms = meta.timestamp.sec as f64 * 1e3 + meta.timestamp.usec as f64 / 1e3;
                let options = zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGB);
                let mut dec = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(mjpeg), options);
                let Ok(px) = dec.decode() else { continue };
                if px.len() != n * 3 {
                    continue;
                }

                #[cfg(feature = "calibrate")]
                if shared.scan_requested.swap(false, Ordering::Relaxed) {
                    if cal_settings.is_none() {
                        let (ok, s) = chameleon::get_settings();
                        if ok {
                            cal_settings = Some(s);
                        }
                    }
                    let result = match &cal_settings {
                        None => Err("chameleon settings unavailable (~/.config/Verichrome/settings.cfg)".to_string()),
                        Some(settings) => {
                            let mut linear: Vec<f32> = px.iter().map(|&v| (v as f32 / 256.).powi(2)).collect();
                            match chameleon::scan_target(&mut chameleon::ImageData::F32Data(&mut linear), &mut cal_info, settings, true) {
                                Some((_, _, _, report, warning, live, cal)) => {
                                    // chameleon live's normalization: white to the weakest row, then the green row pulled to 0.8.
                                    let m = &mut cal_info.cam2terminal9;
                                    let wr = m[0] + m[1] + m[2];
                                    let wg = m[3] + m[4] + m[5];
                                    let wb = m[6] + m[7] + m[8];
                                    let scale = 1. / wr.min(wg).min(wb);
                                    for v in m.iter_mut() {
                                        *v *= scale;
                                    }
                                    for v in &mut m[3..6] {
                                        *v *= 0.8;
                                    }
                                    {
                                        let mut s = shared.matrix.lock().unwrap();
                                        for r in 0..3 {
                                            for c in 0..3 {
                                                s[r][c] = m[r * 3 + c];
                                            }
                                            s[r][3] = 1.;
                                        }
                                    }
                                    if let Some(lo) = live {
                                        let (x0, y0, w, h, rgba) = lo.warped();
                                        // RGB input: the overlay comes back at 2× — bin it to frame scale.
                                        let rgba = bin2_rgba(&rgba, w, h);
                                        overlay = Some((x0 / 2, y0 / 2, w / 2, h / 2, rgba));
                                        shared.overlay_frames.store(45, Ordering::Relaxed);
                                    }
                                    let readout = match cal {
                                        Some(c) => format!("cal: gamma {:.2}  IR {:+.1}% {:+.1}% {:+.1}%  UV {:.1}%  target #{} life {:.0}%{}", c.gamma, c.ir[0] * 100., c.ir[1] * 100., c.ir[2] * 100., c.uv * 100., c.serial, c.life * 100., if c.warning.is_empty() { String::new() } else { format!("  ⚠ {}", c.warning.trim()) }),
                                        None => "cal: scanned".to_string(),
                                    };
                                    let mut text: String = report.iter().map(|s| s.to_string()).collect();
                                    text.push_str(&warning);
                                    Ok(ScanReport { readout, report: text })
                                }
                                None => Err(format!("scan rejected: {}", chameleon::get_last_scan_error().unwrap_or_else(|| "no target found".into()))),
                            }
                        }
                    };
                    send(LiveMsg::Scan(result));
                }

                let m = shared.applied();
                let clip = shared.clip.load(Ordering::Relaxed);
                let ev_gain = f32::from_bits(shared.ev_gain.load(Ordering::Relaxed));
                let hdr = shared.hdr.load(Ordering::Relaxed);
                let show_overlay = shared.overlay_frames.load(Ordering::Relaxed) > 0;
                if show_overlay {
                    shared.overlay_frames.fetch_sub(1, Ordering::Relaxed);
                }
                let want_frame = shared.viewfinder.load(Ordering::Relaxed) && !shared.frame_pending.load(Ordering::Relaxed);
                // Full-resolution viewfinder. The buffers are reused across frames and only taken (moved out, reallocated lazily next frame) when the UI wants one; no per-row allocation.
                if want_frame {
                    lin_buf.resize(n * 3, 0);
                    codes_buf.resize(n * 3, 0);
                }
                use rayon::prelude::*;
                let row_w = WIDTH * 3;
                // One parallel pass over output rows (rotated 180°): linearize, overlay, matrix, then the loopback byte; the viewfinder rows (when wanted) are written in the same pass from the same linear values.
                let work = |h: usize, orow: &mut [u8], lrow: Option<&mut [i32]>, crow: Option<&mut [u16]>| {
                    let (mut lrow, mut crow) = (lrow, crow);
                    for w in 0..WIDTH {
                        let (sh, sw) = (HEIGHT - 1 - h, WIDTH - 1 - w);
                        let s = (sh * WIDTH + sw) * 3;
                        let (mut r, mut g, mut b) = (lin_lut[px[s] as usize] as f32, lin_lut[px[s + 1] as usize] as f32, lin_lut[px[s + 2] as usize] as f32);
                        if show_overlay {
                            if let Some((x0, y0, ow, oh, ov)) = &overlay {
                                if sw >= *x0 && sw < x0 + ow && sh >= *y0 && sh < y0 + oh {
                                    let i = ((sh - y0) * ow + (sw - x0)) * 4;
                                    let a = ov[i + 3];
                                    if a > 0. {
                                        r = ov[i] * 65535. * a + r * (1. - a);
                                        g = ov[i + 1] * 65535. * a + g * (1. - a);
                                        b = ov[i + 2] * 65535. * a + b * (1. - a);
                                    }
                                }
                            }
                        }
                        let (tr, tg, tb) = (m[0] * r + m[1] * g + m[2] * b, m[3] * r + m[4] * g + m[5] * b, m[6] * r + m[7] * g + m[8] * b);
                        // The stream's encode boundary, the viewer's rules in the viewer's order: EV gain → clip indicator (pre-rail) → clamp → HDR rail → transfer.
                        let enc = |v: f32| -> u8 {
                            let v = v * ev_gain;
                            if clip {
                                if v >= 65535. {
                                    return enc_lut[0];
                                }
                                if v < 0. {
                                    return enc_lut[65535];
                                }
                            }
                            let i = v.clamp(0., 65535.) as i64;
                            enc_lut[if hdr { crate::convert::hdr_rail(i) } else { i } as usize]
                        };
                        orow[w * 3] = enc(tr);
                        orow[w * 3 + 1] = enc(tg);
                        orow[w * 3 + 2] = enc(tb);
                        if let Some(l) = lrow.as_deref_mut() {
                            l[w * 3] = tr as i32;
                            l[w * 3 + 1] = tg as i32;
                            l[w * 3 + 2] = tb as i32;
                        }
                        if let Some(c) = crow.as_deref_mut() {
                            c[w * 3] = px[s] as u16;
                            c[w * 3 + 1] = px[s + 1] as u16;
                            c[w * 3 + 2] = px[s + 2] as u16;
                        }
                    }
                };
                if want_frame {
                    out.par_chunks_mut(row_w).zip(lin_buf.par_chunks_mut(row_w)).zip(codes_buf.par_chunks_mut(row_w)).enumerate().for_each(|(h, ((orow, lrow), crow))| work(h, orow, Some(lrow), Some(crow)));
                } else {
                    out.par_chunks_mut(row_w).enumerate().for_each(|(h, orow)| work(h, orow, None, None));
                }
                if let Some(s) = stdin.as_mut() {
                    if s.write_all(&out).is_err() {
                        stdin = None;
                        send(LiveMsg::Status(format!("live: {LOOPBACK} stream ended — viewfinder only")));
                    }
                }
                if let Some(r) = recorder.as_mut() {
                    // CFR conform on the wall clock: this frame stands for every output slot that has come due (a slow camera duplicates), and none if its slot hasn't (a fast one drops).
                    let now = wall_ms();
                    let mut ok = true;
                    while rec_next_due <= now + FRAME_MS / 2. && ok {
                        ok = r.write(&out);
                        if ok {
                            shared.rec_frames.fetch_add(1, Ordering::Relaxed);
                        }
                        rec_next_due += FRAME_MS;
                    }
                    if !ok {
                        let p = r.path.clone();
                        *shared.audio_feed.lock().unwrap() = None;
                        recorder.take().unwrap().finish();
                        *shared.record.lock().unwrap() = None;
                        send(LiveMsg::Status(format!("live: recording ended (encoder closed) → {}", p.display())));
                    }
                }
                // opsin's share of the A/V delay: capture stamp → handed to ffmpeg. EMA so the readout doesn't flicker; a nonsense stamp (a driver on another clock) is ignored.
                let pipe = monotonic_ms() - captured_ms;
                if pipe > 0. && pipe < 2000. {
                    let prev = shared.pipe_ms10.load(Ordering::Relaxed) as f64 / 10.;
                    let ema = if prev == 0. { pipe } else { prev * 0.9 + pipe * 0.1 };
                    shared.pipe_ms10.store((ema * 10.) as u32, Ordering::Relaxed);
                }
                frame_no = frame_no.wrapping_add(1);
                if frame_no % 30 == 0 {
                    if let Some(m) = mic.as_mut() {
                        m.set_delay(shared.av_delay_ms());
                    }
                }
                if want_frame {
                    shared.frame_pending.store(true, Ordering::Relaxed);
                    send(LiveMsg::Frame(Frame { w: WIDTH, h: HEIGHT, lin: std::mem::take(&mut lin_buf), codes: std::mem::take(&mut codes_buf) }));
                }
            }
            drop(stdin);
            if let Some(r) = recorder.take() {
                *shared.audio_feed.lock().unwrap() = None;
                r.finish();
            }
            drop(mic);
            if let Some(mut f) = ffmpeg {
                let _ = f.kill();
                let _ = f.wait();
            }
        })
        .map_err(|e| e.to_string())
}
