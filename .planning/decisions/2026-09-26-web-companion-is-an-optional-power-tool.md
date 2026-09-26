# The web companion is an optional power tool

**Date:** 2026-09-26 · **Status:** Accepted · **Source:** vision session with Andreas

## Context
The vision promises "no app, no driver" on the host and makes the screen the
product's identity. WebUSB over the iface-6 config channel is proven (EQ import,
`pico-link-ryw.12`), which opens the door to a browser UI alongside the device.

## Decision
Build a web companion as an **optional power tool**. The dongle stays fully usable
without it: every function the device offers stays reachable from the buttons; the
web only adds what 240x240 + a d-pad does badly. Host it as a public HTTPS static
site, advertised via the WebUSB landing-page descriptor. Ship in order: EQ editor +
preset library, AutoEQ search, live diagnostics, firmware update (after a
PICOBOOT feasibility spike).

## Rationale
Keeps the "no app needed" promise and the screen-first identity intact while
removing the worst on-device ergonomics (curve editing, preset libraries,
database search). EQ first because its transport is proven; firmware update last
because picotool already fails on this Mac and release builds lack remote reboot.

**Amendment (same day, Andreas):** the companion *visually* mirrors the device.
Its home page reuses the device's home layout, including the live moving level
bars. This is a mirror of the visual language, not functional parity. Live bars
need a small device-to-host telemetry stream (meter levels, link state), so that
stream moves into F1.

**Amendment 2 (same day, Andreas):** the whole companion is **web-native**. The
home page reuses the device's *layout* (and live bars) at browser resolution; it
is not a pixel clone. A wasm build of `core/` rendering the real 240x240
framebuffer was designed and rejected: an LCD-constrained page cannot deliver
the better EQ experience that is the companion's reason to exist, and a page
mixing a scaled LCD with native controls is incoherent. The telemetry snapshot
carries resolved values (kbps, names, FX) so the page reimplements layout, not
logic; only the meter hold/decay is recomputed in JS.

## Alternatives
- **Full functional mirror** — two equal front-ends; doubles every feature and dilutes the screen. (Rejected; the visual mirror above is different.)
- **Web primary** — changes what the product is.
- **Setup-only** — too narrow to justify the hosting and WebUSB work.
- **Local-only hosting** — no plug-in-and-go, no AutoEQ fetch.

## Consequences
- Chromium-only for the companion; acceptable because it is optional.
- A device feature that only the web exposes is a bug against this ADR.
- Diagnostics must budget USB bandwidth against the audio stream.
- `pico-link-ryw.12.7` folds into F1.
