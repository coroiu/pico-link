# Pico Link web companion

Vite + React + TypeScript. Not a Cargo workspace member and not built by the
firmware toolchain -- its only coupling to `core/` is committed fixture files
(`fixtures/telemetry/`, landing on `pico-link-jyhk.9`) plus hand-ported
constants that a test asserts against. See the FERN DESIGN comment on
`pico-link-jyhk.8` for the full design.

## Develop

```bash
npm install
npm run dev      # http://localhost:5173, runs against FakeTransport (see below)
npm test         # vitest
npm run build    # tsc -b && vite build -> dist/
npm run lint     # oxlint
```

`vite.config.ts` sets `base: "./"` so the built `dist/` works from any static
host path (a GitHub Pages project subpath, a plain file server, etc).

## No hardware needed

The `Transport` interface (`src/transport/types.ts`) is the seam that makes
the page hardware-optional:

- `FakeTransport` (`src/transport/fake.ts`) -- a scripted device model, the
  dev page's default. Injectable latency and stalls.
- `ReplayTransport` (`src/transport/replay.ts`) -- plays back a captured real
  session (`tools/usb-console`'s pyusb poller, `--record`) with no board
  attached.
- `WebUsbTransport` (`src/transport/webusb.ts`) -- real hardware over WebUSB.
  Stub only; the session layer (chooser, reconnect, single-flight queue,
  pacing) is `pico-link-jyhk.11`.

`src/dev/DevPage.tsx` is the current (only) route: it runs against
`FakeTransport`, decodes the Home telemetry snapshot with the typed decoder
in `src/proto/telemetry.ts`, and shows a theme switcher plus a minimal debug
readout. This is the page's headless run mode -- screenshot it with headless
Chrome and zoom-inspect, per the repo's `CLAUDE.md` rendering-verification
rule. No Home visuals live here yet; that's `pico-link-jyhk.12`, pending
Uma's design.

## Wire protocol

`src/proto/telemetry.ts` and `src/proto/info.ts` are hand-ported TS mirrors
of `core/src/app/telemetry.rs`'s `HomeSnapshot` wire format and the C
`pl_cfg_info_wire_t` GET_INFO struct respectively. Per the design's "core
emits, JS asserts" rule, nothing here is a second source of truth -- once
`pico-link-jyhk.9` lands `fixtures/telemetry/`, this module's tests move to
assert against those committed fixtures instead of only round-tripping
locally encoded test data.

## Theming

Three themes selected by `data-theme` on `<html>` (`src/theme/
ThemeProvider.tsx`): `device` (default), `light`, `dark`. All theme colours
live in one place, `src/index.css`, as CSS custom properties consumed
through Tailwind's `@theme inline` -- swapping the palette (see
`pico-link-5ful`, the palette is being redesigned) is a one-file change.
`device`'s values are currently hand-converted from `core/src/render/
theme.rs`'s Rgb565 palette (see the comment above `:root` in `index.css`); a
fixture-exported source of truth is a follow-up.

The meter canvas (`src/dev/MeterCanvas.tsx`) reads colours via
`getComputedStyle` at draw time, so it repaints correctly on a theme switch
without React state in the hot path -- meters and (later) the EQ curve
always draw on canvas via `requestAnimationFrame`, never through React state
at 30Hz (design requirement).
