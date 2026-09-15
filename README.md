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

**View.** `opsin file.vsf` (or any supported RAW) opens the [fluor](https://github.com/nickspiker/fluor) viewer — one instance: while a viewer is running, every later `opsin file` hands the file to that window (a Unix socket in `$XDG_RUNTIME_DIR`) and returns in a millisecond, and the window surfaces where the pointer is; bare `opsin` just raises it. The view transform is span-relative: resize the window from any edge and the composition scales with it like every other UI element, no modes. Drag to pan, wheel to zoom around the cursor. The magnification readout truncates, never rounds, and shows the bare `1x` only when pixel-exactness is bitwise true. Right panel: navigator with live view rect, per-channel log histogram with open-interval clipping bins, MacLeod-Boynton chromaticity chart computed from the Stockman & Sharpe 2000 10° cone fundamentals with the Planckian arc integrated from Planck's law, exposure slider working in signed linear (clipped speculars and sub-black noise stay recoverable). The histogram tracks the exposure slider through a gain remap of the same raw counts (2× per stop in linear, a translation in log — never a curve), and the Clip toggle is lumis's preview_sub inversion at the encode boundary: whatever clips at DISPLAY under the current exposure renders inverted, channel-wise, live with the slider. Neither ever touches the data; the JPEG export stays clean of the indicator by construction.

**IDT copy/paste.** A VERICHROME Direct Scene Referred IDT is a property of the sensor, so one chameleon scan characterizes every frame that sensor ever shot. `Ctrl+C` lifts the current DNG's magic-9 (ColorMatrix1 + illuminant, exact rationals — no float round trip) with the camera's fingerprint into `~/.config/opsin/idt.clip`, a readable nine-numbers text you can keep or share; `Ctrl+V` patches it into the current frame's own ColorMatrix1 slot in place — same byte count, nothing else in the file moves, the sensor plane untouched. Paste refuses unless make, model, raw dims, CFA tile, and focal length all agree, naming the fields that don't. Two tiers in the refusal: focal length and raw dims are the soft one — a lens change on an interchangeable-lens body, a crop mode, or lumis's 2×-height slitscan ring all keep the sensor, so the IDT is still valid (though on a phone a focal change means a different camera module: main/ultrawide/tele are different sensors behind one Make/Model); a different CFA tile or camera name is the hard one. Either can be overridden — `Ctrl+Shift+V` / `--force` — and the report then carries a WARNING naming what was overridden, so the decision is on record. Headless for a whole folder: `opsin --copy-idt donor.dng && opsin --paste-idt [--force] shots/*.dng` (each frame judged on its own fingerprint).

**Frame info.** A HUD over the image's bottom-left (`I` toggles): file, camera, sensor dims, CFA tile, focal/aperture/exposure/ISO, black/white levels, orientation, and the IDT — source, trust grade, class, illuminant, profile name, the nine numbers themselves (or "none — uncalibrated" when the DNG carries lumis's identity sentinel, which now renders raw-camera instead of masquerading as a characterization). Below that the live state (EV, zoom, clip) and, under the cursor, the image pixel, every raw ADC code in its sensor tile by channel, and the linear display value — readings, never interpretations.

**Keys.** Arrows navigate the folder. `V` converts the current image. `Ctrl+C`/`Ctrl+V` copy/paste the IDT (`Ctrl+Shift+V` forces). `I` toggles the frame-info HUD. `E` exports the current view as an sRGB JPEG beside the source — live exposure baked in, Rec.2020→sRGB at the encode boundary, the one legacy-space concession for platforms that assume sRGB. `F`/`1` fit and 1:1. `+`/`-`/`0` nudge and reset exposure. Hold `[` `]` and tap a letter for the debug overlays (hitmask, alpha, damage, fps).

Formats in: `vsf` native, DNG and the common camera RAWs through [iris](https://github.com/nickspiker/iris), and the display-referred pair JXL + JPEG (lumis exports and web files) — JXL's tagged colour encoding, or JPEG's assumed-sRGB convention, becomes an `assumed`-grade profile entry, transfer un-done to linear at ingest.

## Landing

- Minimal edits, recorded as `view_transform` metadata: rotation, starring, culling, and the light creative ops (the vocabulary is reserved in the spec: `curve`, `contrast`, `skew_matrix`, `dr_curve`).
- The observer machinery itself: the Inspect state, where the standard observer becomes a live control.
- `unit`-grade characterization: chameleon target scans attaching magic-9 profiles to matching sources.
- Assumed-observer ingest (JPEG/PNG/TIFF/PSD) and legacy export (DNG/TIFF escape hatches), both arriving via iris as the format gateway grows.

## Scope, deliberately

opsin is the viewer and converter, with the minimal edits a viewer earns. It will not become the editor; full editing (the heavy author of the same `view_transform` log) is a separate future crate speaking the same format. Everything opsin writes, it writes as metadata over untouched sensor data.

## License

VSF License (see LICENSE).
