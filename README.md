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

**HDR.** The `HDR` pill (`H`) applies a highlight rolloff at the encode boundary — Photon's audio wire shaper on the u16 display domain, `y = (3x − (x³ >> 32)) >> 1`, integer and branchless, per channel: slope 3/2 at black, tangent to white, so the clamp lands where the curve is already flat, and its only distortion product is 3rd-order. It brightens on its own; pull exposure down ~3× and about 1.5 stops that used to clip roll off instead. Screen and JPEG share the one curve, so the export is the screen. It's a Creative op and treated as one: never baked into a plane, recorded as a `dr_curve` view op (polynomial coefficients) on VSF convert, and named in the HUD.

**Crop and rotate.** `Crop` (`C`) arms a crop: the rect seeds as the full frame and the view fits; click or drag anywhere on the image and the NEAREST corner comes to the cursor — no handles, no modes; outside the rect dims, the navigator shows where it sits, the HUD reads the numbers. Armed, `JPEG` exports exactly the rect and `Fit` fits it; toggle off and it's gone. `CCW`/`CW` (`R`/`r`) turn the display 90° — composed onto the orientation view op and re-rendered from the retained decode, the crop riding along. Both are view ops: `V` records `orientation` and `crop [x, y, w, h]` (display pixels, after orientation) in the translateration log; the plane is never touched.

**Keys.** Arrows navigate the folder. `V` converts the current image. `Ctrl+C`/`Ctrl+V` copy/paste the IDT (`Ctrl+Shift+V` forces). `I` toggles the frame-info HUD. `H` toggles HDR. `C` toggles crop, `r`/`R` rotate CW/CCW. `E` exports the current view as an sRGB JPEG beside the source — live exposure and HDR baked in, Rec.2020→sRGB at the encode boundary, the one legacy-space concession for platforms that assume sRGB. `F`/`1` fit and 1:1. `+`/`-`/`0` nudge and reset exposure (range −4…+12 stops — a sensor holds ~12 above the floor, and the signed-linear pipe keeps them all). Hold `[` `]` and tap a letter for the debug overlays (hitmask, alpha, damage, fps).

Formats in: `vsf` native, DNG and the common camera RAWs through [iris](https://github.com/nickspiker/iris), and the display-referred trio JXL + JPEG + WebP (lumis exports and web files) — JXL's tagged colour encoding, or the assumed-sRGB convention of JPEG and WebP, becomes an `assumed`-grade profile entry, transfer un-done to linear at ingest. A WebP with alpha composites over black; an animated one opens on its first frame.

## Landing

- Minimal edits, recorded as `view_transform` metadata: rotation, starring, culling, and the light creative ops (the vocabulary is reserved in the spec: `curve`, `contrast`, `skew_matrix`, `dr_curve`).
- The observer machinery itself: the Inspect state, where the standard observer becomes a live control.
- `unit`-grade characterization: chameleon target scans attaching magic-9 profiles to matching sources.
- Assumed-observer ingest (PNG/TIFF/PSD) and legacy export (DNG/TIFF escape hatches), both arriving via iris as the format gateway grows.

## Scope, deliberately

opsin is the viewer and converter, with the minimal edits a viewer earns. It will not become the editor; full editing (the heavy author of the same `view_transform` log) is a separate future crate speaking the same format. Everything opsin writes, it writes as metadata over untouched sensor data.

## License

VSF License (see LICENSE).
