# Overlay tuner

A standalone, self-contained web page for designing the recording overlay (the
dictation pill) **without rebuilding the app**. Live preview + real microphone +
sliders for every dimension; its "Config" readout maps 1:1 to the values in
`src/overlay/RecordingOverlay.css` / `RecordingOverlay.tsx`.

## Run it

The real microphone needs a secure context, so serve it over `localhost` (a
`file://` open works for layout but Safari blocks the mic there):

```bash
python3 -m http.server 8787 --bind 127.0.0.1 \
  --directory tools/overlay-tuner
# then open http://localhost:8787/
```

- Click **Use my microphone** (auto-starts once granted for localhost) → the
  waveform reacts to your voice.
- Pick a preset (Current / Small / Tiny / Micro), then fine-tune with the sliders.
  Settings persist across refreshes (localStorage).
- When happy, hit **Copy config** and hand the values to whoever is editing the
  overlay CSS.

## Porting a config into the app

The readout keys map directly:

| Tuner key | Overlay CSS |
|---|---|
| `--ov-rest-w` / `--ov-work-w` / `--ov-base-h` | same vars in `RecordingOverlay.css` `:root` |
| border-radius | `.scard` / `.scard.working` |
| accent light/dark | theme `--color-logo-primary` (light/dark) |
| wave bars | `WAVE_BARS` in `RecordingOverlay.tsx` |
| bar w/gap/max | `.swave i` width, `.swave` gap, `.swave i` max-height |
| dot size / close btn | `.sdot`, `.sx` |
| label size | `.swork-label` |

The live waveform animation (peak of the voice band, √-curve, centre envelope)
lives in `RecordingOverlay.tsx` — see `MIC_GAIN` / `BAR_MIN` / `BAR_MAX`.

> Dev tool only — not part of the app build. It stays in sync by hand; if you
> change the overlay CSS, mirror the defaults here so the preview stays honest.
