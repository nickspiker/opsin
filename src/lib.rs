//! opsin as a library: the ingest + colour pipeline ([`convert`]) and the display encode ([`render`]), for hosts that draw the pixels themselves (photon's attachment viewer). The viewer binary lives behind the `viewer` feature.
pub mod convert;
pub mod idt;
pub mod tiff;
pub mod render;
