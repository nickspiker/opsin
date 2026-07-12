<p align="center">
  <img src="https://raw.githubusercontent.com/nickspiker/opsin/main/opsin.webp" alt="opsin, the observer is a variable" width="512">
</p>

# opsin

Spectral image viewer and converter. Named for the photopigment proteins that define an observer's spectral response, because in opsin **the observer is a variable**.

## The model

An image is K channels of sensor counts, and each channel carries its own spectral sensitivity curve (self-describing wavelength grid, conventionally 350 to 1100nm). A Bayer RAW is K=3 with camera curves; an LED multispectral composite is K=25 with LED×sensor product curves. RGB under CIE 1931 is one possible *rendering*, resolved at view/export time against whatever observer you choose, never the storage model. DNG stores the answer; VSF-Image stores the question.

The container is [VSF](https://github.com/nickspiker/vsf) (`vsf::spectral_image`): sensor counts bitpacked at native depth, plus the sections that carry meaning without touching data:

- `spectral_response`: per-channel sensitivity curves, the gold tier of characterization.
- `colour_profile`: tiered camera→VSF-RGB matrices, best entry first, each labeled with its source, IDT class, and trust grade (`unit` measured on this camera, `model` factory, `assumed` format convention). The raw questions ride alongside: verbatim DNG matrices, solve patches, calibration provenance.
- `view_transform`: the translateration log. Ordered, named view ops with parameters, replayed by any reader. The pixels never change; the interpretation travels.
- `provenance`: ihi identity ingredients.

opsin does **translateration**: it moves images between formats and observers without ever baking an interpretation into the data. Display is derived fresh every frame (stored camera→VSF-RGB matrix concatenated with VSF-RGB→Rec.2020, illuminant-normalized); nothing display-space is ever written to a file.

## Today

**Convert.** `opsin --convert shot.dng` writes `shot.vsf`: counts untouched at native depth, both DNG ColorMatrices preserved verbatim, daylight-characterized entry elected first. A converted file reopens rendering bit-identically to its source.

**View.** `opsin file.vsf` (or any supported RAW) opens the [fluor](https://github.com/nickspiker/fluor) viewer. The view transform is span-relative: resize the window from any edge and the composition scales with it like every other UI element, no modes. Drag to pan, wheel to zoom around the cursor. The magnification readout truncates, never rounds, and shows the bare `1x` only when pixel-exactness is bitwise true. Right panel: navigator with live view rect, per-channel log histogram with open-interval clipping bins, MacLeod-Boynton chromaticity chart computed from the Stockman & Sharpe 2000 10° cone fundamentals with the Planckian arc integrated from Planck's law, exposure slider working in signed linear (clipped speculars and sub-black noise stay recoverable).

**Keys.** Arrows navigate the folder. `V` converts the current image. `F`/`1` fit and 1:1. `+`/`-`/`0` nudge and reset exposure. Hold `[` `]` and tap a letter for the debug overlays (hitmask, alpha, damage, fps).

Formats in: `vsf` native, plus DNG and the common camera RAWs through [iris](https://github.com/nickspiker/iris).

## Landing

- Minimal edits, recorded as `view_transform` metadata: rotation, starring, culling, and the light creative ops (the vocabulary is reserved in the spec: `curve`, `contrast`, `skew_matrix`, `dr_curve`).
- The observer machinery itself: the Inspect state, where the standard observer becomes a live control.
- `unit`-grade characterization: chameleon target scans attaching magic-9 profiles to matching sources.
- Assumed-observer ingest (JPEG/PNG/TIFF/PSD) and legacy export (DNG/TIFF escape hatches), both arriving via iris as the format gateway grows.

## Scope, deliberately

opsin is the viewer and converter, with the minimal edits a viewer earns. It will not become the editor; full editing (the heavy author of the same `view_transform` log) is a separate future crate speaking the same format. Everything opsin writes, it writes as metadata over untouched sensor data.

## License

VSF License (see LICENSE).
