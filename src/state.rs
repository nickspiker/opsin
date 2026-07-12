//! Viewer state machine, Photon-style. Each state gets its own layout + render arm in the eventual OpsinApp (fluor FluorApp impl).

#[allow(dead_code)]
pub enum AppState {
    /// Folder grid of thumbnails.
    Browser,
    /// Single image: pan/zoom over a mip pyramid, tiles clipped to damage rect.
    View,
    /// Pixel spectra, CIE plot / blackbody / histogram (ported from oriel), provenance chain with verification pips.
    Inspect,
    /// Batch conversion queue.
    Convert,
}
