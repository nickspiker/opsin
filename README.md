# opsin

Spectral image viewer, converter and (eventually) editor. Named for the photopigment proteins that define an observer's spectral response — because in opsin, **the observer is a variable**.

## The model

An image is K channels of sensor counts, and each channel carries its own spectral sensitivity curve (self-describing wavelength grid, conventionally 350–1100nm). A Bayer RAW is K=3 with camera curves; an LED multispectral composite is K=25 with LED×sensor product curves. RGB under CIE 1931 is one possible *rendering*, resolved at view/export time against whatever observer you choose — never the storage model. DNG stores the answer; VSF-Image stores the question.

The container is [VSF](../vsf) (`vsf::spectral_image`, sections: `spectral_image`, `spectral_response`, `provenance`), with sensor counts bitpacked at native depth and ihi provenance ingredients carried alongside (handle omission is `""`, by design).

## 0.0.0: headless converter

```
opsin --convert shot.ARW shot.vsf      # ingest: camera RAW → VSF-Image (50+ formats via chameleon/dng_io)
opsin --convert shot.vsf shot.dng      # export: legacy escape hatch, 16-bit mosaic DNG
opsin --convert shot.vsf shot.tif      # export: 16-bit RGB TIFF rendering (CFA-binned half-res, no invented interpolation)
```

Ingest→export→ingest round-trips bit-identically.

## Viewer

`opsin file.vsf` opens the fluor viewer (CPU softbuffer, DefaultChrome): fit-to-window on open, drag to pan, wheel to zoom around the cursor, `F` refits, `1` goes 1:1, Escape closes. Rendering is raw camera space through a gamma-2 display encode — no observer applied yet; the observer machinery is the point of the Inspect state to come (Browser / Inspect / Convert states are scaffolded in `state.rs`).

The right tool panel (divider draggable with the OS ew-resize arrows; width is a fraction of the window) stacks a navigator (click to jump, white rect tracks the view), per-channel log histogram, and a MacLeod–Boynton chromaticity chart whose spectral locus is computed at startup from the Stockman & Sharpe 2000 10° cone fundamentals in `vsf::colour` — with the Planckian arc integrated from Planck's law against the same curves, and the image's chromaticity cloud over it (camera RGB via VSF primaries for now — an approximation until the spectral resolve lands).
