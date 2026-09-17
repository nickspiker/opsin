//! opsin as a library: the ingest + colour pipeline ([`convert`]), the display encode ([`render`]), and — behind the `view` feature — the whole viewer interface as an embeddable component ([`view::View`], what photon's attachment viewer IS). The window binary lives behind the `viewer` feature.
pub mod convert;
pub mod headerless;
pub mod sniff;
pub mod icc;
pub mod idt;
pub mod tiff;
pub mod render;
/// The embeddable viewer — see [`view::View`]. Behind the `view` feature (fluor without a window host).
#[cfg(feature = "view")]
pub mod view;
#[cfg(feature = "view")]
pub mod panel;
#[cfg(feature = "live")]
pub mod live;
#[cfg(feature = "calibrate")]
pub mod calibrate;
