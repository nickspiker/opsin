//! One-shot: wrap a 256×256 raw RGB blob into the VSF orb format `fluor::host::icon::Icon::from_vsf_bytes` expects — an `image` section with a `data` field holding a `t_u3` tensor of shape `[256, 256, 3]` (VSF-RGB gamma2, i.e. the stored bytes are the visible values the α+darkness decoder inverts). Run once to regenerate `assets/opsin_orb.vsf` after editing the source art:
//!   magick opsin.png -resize 256x256! -depth 8 rgb:assets/opsin_orb.rgb
//!   cargo run --bin make_orb

use vsf::{Tensor, VsfBuilder, VsfType};

fn main() {
    let rgb = std::fs::read("assets/opsin_orb.rgb").expect("read assets/opsin_orb.rgb (magick -resize 256x256! rgb:)");
    assert_eq!(rgb.len(), 256 * 256 * 3, "expected 256×256×3 raw RGB, got {} bytes", rgb.len());

    let vsf = VsfBuilder::new()
        .add_section("image", vec![("data".to_string(), VsfType::t_u3(Tensor::new(vec![256, 256, 3], rgb)))])
        .build()
        .expect("build orb VSF");

    std::fs::write("assets/opsin_orb.vsf", &vsf).expect("write assets/opsin_orb.vsf");
    println!("wrote assets/opsin_orb.vsf ({} bytes)", vsf.len());
}
