# 06 — Documentation vs reality, build system, engineering process

Reviewer seam: `docs`. Tree: `2e37164` (main tip, 2026-09-24). Reviewed 2026-09-25.

Note on the baseline: the container arrived as a **shallow clone (132 commits, boundary
2026-09-06)**. I ran `git fetch --deepen=2000 origin main`; the full history is **504
commits, 2026-08-26 → 2026-09-24** (244 in August, 260 in September). Every hash cited by
the docs resolves. The charter's "132 commits" is a shallow-clone artefact, not the
project's size; the ratio is 39 design docs + 7 ADRs to 504 commits.

Measured here: `cargo test --workspace` **578 passed, 0 failed** (core 478, ui-ffi 58,
emulator 27+2+3+3, core integration 1+1+5). `cargo clippy --workspace --all-targets`
**7 warnings**. `cargo fmt --all --check` **FAILS, 17,295-line diff** (no `rustfmt.toml`;
formatting is not enforced anywhere). Host C tests: **7 of 8 build and pass** with their
own documented `cc` lines; the 8th (`test_paired_device_upserted_ldac_quality_echo.c`)
needs the cbindgen-generated `firmware/include/pico_link_ui.h`, which a fresh clone does
not have.

---

## 1. Verdict

The *design* layer (`.planning/design/`, `.planning/decisions/`) is unusually good:
file:line-anchored, self-correcting, honest about what was measured vs judged. The
*status* layer is uniformly stale — `CLAUDE.md` "Current State" (2026-08-28),
`roadmap.md` (2026-08-28), `progress.md` (2026-08-31), `README.md` (2026-08-27) and both
INDEX status columns describe a project four weeks and ~260 commits behind the tree. A
cheaper model that trusts them will re-do merged work, chase resolved bugs, and cross-compile
for the wrong target triple. At its current size the documentation set is an **asset for
design and a liability for status**; the fix is small (one status surface, kept current at
session close) rather than "write less". There is no CI, no LICENSE, and the process rules
in `CLAUDE.md` ("merge when CI green", "every task through beads, no exceptions") are not
what the git history shows.

## 2. What is well done — do not touch

- **ADR discipline is real.** `decisions/INDEX.md` marks 2026-08-26 "Rust owns the binary"
  as **Superseded** by 2026-08-27 C-first, marks 2026-09-02 as "Partially amended", and the
  2026-09-03 ADR carries its own §11/§12 re-scope with the flip date. Superseded entries are
  never deleted. The 2026-08-11 ADR is honestly labelled "written retroactively".
- **Design docs cite `file:line`, record corrections inline, and name their bead.** E.g.
  `2026-09-23-on-arm-ring-collapse.md` ("Status: Live, implemented on bd-pico-link-rzqd"),
  `2026-09-24-app-rs-split.md` whose S0–S7 steps match the commits `1aa5cd7`…`225db04`
  one-for-one and whose target tree matches `core/src/app/` today.
- **The SDK-patch enforcement is the right shape.** `firmware/CMakeLists.txt:15-119` reads the
  SDK source and `FATAL_ERROR`s on a missing marker; `tools/apply-sdk-patches.sh` is
  idempotent and refuses unknown SDK versions ("matches neither the stock nor the patched
  form"). This is the one place the build is genuinely reproducible across machines.
- **`firmware/vendor/libldac/PROVENANCE.md`** pins upstream commit
  `eeee1a3f5f8df1282e3a6d297085885fd886737b`, lists exactly what was vendored, and states
  "no byte of USBPods went into this". Licence file and NOTICE are copied verbatim.
- **The cbindgen header is generated, never committed** (`firmware/.gitignore`,
  `CMakeLists.txt:202-210`), so the C/Rust ABI cannot hand-drift.
- **`Cargo.lock` is committed**; workspace `default-members` keeps `ui-ffi` out of host
  builds with a clear comment (`Cargo.toml:4-8`).
- **`.research/captures/2026-08-31-ldac-dwell-fix/README.md`** is a model evidence record:
  what each capture proves, and the two parsing traps that already produced wrong readings.
- **`tools/usb-console/README.md`** records what was *measured* on 2026-08-27, not what was
  assumed.

## 3. Architecture assessment (of the documentation system itself)

The project has four layers of written record and they disagree about who owns "status":

| Layer | Files | Currency (last commit) | Role it claims |
|---|---|---|---|
| Instructions | `CLAUDE.md` (647 lines), `AGENTS.md`, `.claude/agents/*` (13), `.claude/skills/*` (5), hooks (14) | CLAUDE.md 2026-09-01; agents 2026-09-02; AGENTS.md 2026-08-26 | How agents behave |
| Status | `CLAUDE.md §Current State`, `.planning/progress.md`, `.planning/roadmap.md`, `README.md` | 08-28 / 08-31 / 08-28 / 08-27 | Where things stand |
| Decisions | `.planning/decisions/*` (7) + INDEX | INDEX 2026-09-23 | What was decided |
| Designs | `.planning/design/*` (39) + INDEX | INDEX 2026-09-24 | What to build |
| Evidence | `.research/captures`, `.research/findings/INDEX.md` | captures 08-31; findings INDEX "(none yet)" since 08-26 | What was measured |

Where it diverges and which side is right:
- **Status vs code:** the code is right in every case I checked (§4, F-docs-01…05). The
  status docs were simply not updated after 2026-08-31; nothing in the tree contradicts a
  design, only the *status claims about* designs.
- **Design INDEX vs code:** the INDEX status column lags the merges by up to 17 days
  (F-docs-06). The docs themselves (inside the files) are mostly right; the router is wrong.
- **CLAUDE.md vs CMake:** CLAUDE.md, roadmap and the supervisor agent say
  `thumbv8m.main-none-eabihf`; `CMakeLists.txt:166-181` builds `thumbv8m.main-none-eabi` and
  explains why (softfp ABI). CMake is right; `progress.md:274` already recorded the
  correction on 2026-08-28 but the other three docs were never fixed (F-docs-08).
- **Process rules vs history:** "orchestrator merges when CI green" (CLAUDE.md:93, 108,
  auto-run skill step 4) — there is no `.github/workflows`, so no merge ever met the literal
  rule. "Every task goes through beads. No exceptions" — `git log --first-parent --no-merges
  main` shows 73 commits landed on main outside a merge, ~10 of them code (F-docs-13).

**Is the set an asset or a liability at this size?** ~24,000 lines of markdown against
~6,000 lines of `core/src/app` and ~20k of firmware C. The *design* docs earn their length:
every one I sampled is either implemented as written or carries its own supersession note.
The *status* docs are a liability precisely because they are trusted (the `vision-session`
skill tells Vera to read roadmap+progress as ground truth; `session-resume` says "the
board wins" over memory but names no such rule for `progress.md`). Recommendation (§4,
F-docs-01): one status surface, updated by the `session-close` skill, and delete the
duplicated status prose from `CLAUDE.md`.

---

## 4. Findings

### F-docs-01: `CLAUDE.md` "Current State" is 28 days stale and names six merged/resolved items as open
- Severity: P1   Confidence: High   Effort: S   Tier: Haiku
- Location: `CLAUDE.md:597-647`
- Evidence: the section is headed "2026-08-28 (afternoon)" and "`main` is at `6088202`".
  Git: main is `2e37164` (2026-09-24), 400+ commits later. Stale claims, each verified
  against history (full table below):
  ```
  "pico-link-a67 DONE, unmerged, unreviewed"          -> merged 5416c76 on 2026-08-28
  "Audio: diagnosed, not fixed ... UNPROVEN"          -> 91c2432 2026-08-29 "USB audio streams on macOS"; 6edc201 "A2DP source, SBC audible -- the MVP"
  "Display: half-fixed ... Not merged"                -> 36a6c4f 2026-08-29 merged MADCTL 0x60 (cable-right still open, see st7789.c:171-178)
  "pico-link-6o2 bt.c:102 calls Rust from IRQ"        -> resolved, main.c:644 "(pico-link-6o2) ... makes the real pl_ui_push_event calls here"
  "pico-link-gap panic recorder"                      -> firmware/src/panic_recorder.c exists; fb1a615 "unblind the panic recorder"
  "Next: dispatch a code-reviewer on pico-link-a67, then pico-link-icb on hardware"
  ```
- Why it matters: `session-resume` and `auto-run` read this section first. A model
  following "Next:" literally would review an already-merged branch and re-run a hardware
  hunt that closed on 2026-08-29. The `nudge-claude-md-update.sh` hook only fires when the
  section is *empty*, so a stale-but-present section is never flagged.
- Fix sketch: replace the section with a 5-line pointer ("status lives in
  `.planning/progress.md`; `main` at `<hash>` on `<date>`; next bead `<id>`") and make
  `session-close` step 3 rewrite it. Or delete it: everything it says is also in
  `progress.md`.
- Verification: `grep -n '6088202\|a67 DONE\|half-fixed\|diagnosed, not fixed' CLAUDE.md`
  returns nothing; the section names a hash equal to `git rev-parse --short main`.
- Related: `.claude/hooks/nudge-claude-md-update.sh`, `.claude/skills/session-close/SKILL.md`

**Currency audit (task item 1).** Every row is an agent-facing doc; each stale claim is
one line so it can be fixed independently.

| Doc | Last commit | Current? | Stale claim (quoted) | Reality (evidence) |
|---|---|---|---|---|
| `CLAUDE.md` §Current State | 2026-09-01 (`45a56d4`; section text dated 08-28) | No | "`main` is at `6088202`" | `2e37164`, 2026-09-24 |
| | | | "`pico-link-a67` DONE, unmerged, unreviewed (branch `bd-pico-link-a67`, `1c612b6`)" | merged `5416c76` 2026-08-28 |
| | | | "Audio: diagnosed, not fixed … the fix itself is UNPROVEN because macOS never entered the streaming alt-setting. See `pico-link-icb`" | `91c2432` 2026-08-29 "USB audio streams on macOS"; LDAC listenable 2026-08-31 (`progress.md:3-11`) |
| | | | "Display: half-fixed. `bd-pico-link-zzq` at `8dea0f5` … Not merged" | `36a6c4f` merged 2026-08-29; MADCTL 0x60 in `st7789.c:142` |
| | | | "`pico-link-6o2` (P1, `bt.c:102` calls Rust from IRQ context)" | fixed; `main.c:644` comment; `bt.c` pushes to a ring |
| | | | "`pico-link-gap` (P1, panic recorder — promoted from speculative…)" | `panic_recorder.c` present, `PICO_PANIC_FUNCTION=pl_panic_c_hook` in CMake:444 |
| | | | "`pico-link-yz6`" listed as open | rejected by ADR 2026-09-02 §Decision 1 |
| | | | "`pico-link-d7k`" (no automated input path on real target) | `debug_remote.c` + `PL_DEBUG_REMOTE` (bead cd3) |
| | | | "**Next:** dispatch a code-reviewer on `pico-link-a67` … then `pico-link-icb` on hardware" | both closed in August |
| `CLAUDE.md:51` §Tech Stack | 2026-09-01 | No | "cross-compiles for `thumbv8m.main-none-eabihf`" | `CMakeLists.txt:180` `thumbv8m.main-none-eabi`; `progress.md:274` records the correction |
| `CLAUDE.md:56-69` §Repo layout | 2026-09-01 | Partly | lists `core/`, `emulator/` only | `ui-ffi/`, `firmware/`, `firmware-spike/`, `tools/`, `fixtures/` absent from the map |
| `CLAUDE.md:330-343` §Firmware build | 2026-09-01 | Partly | "flash by holding BOOTSEL … copying the file onto the RP2350 drive" | §Flashing (305-328) says CDC `--bootsel` is now the default path; two sections, two answers |
| `CLAUDE.md:293-301` | | No | "`.research/findings/` — one research finding per file … indexed in `INDEX.md`" | `.research/findings/INDEX.md` says "(none yet)" since 2026-08-26; captures live in `.research/captures/` and are unindexed |
| `.planning/roadmap.md` | 2026-08-28 (`eab223b`) | No | "**Last updated:** 2026-08-28" | 260 commits since |
| | | | "### A — Repo reset (in progress)" | done (progress.md §Epic A ✅) |
| | | | "**M3 IN FLIGHT** (`pico-link-cz0.4`) — TinyUSB composite sound card" | done 2026-08-29 (`91c2432`) |
| | | | "**D1** … Rust owns SPI, DMA and the framebuffer directly now that Rust owns the binary" | contradicts ADR 2026-08-27 cited ten lines earlier; C owns SPI (`st7789.c`) |
| | | | "D2 A narrow, versioned C header we author" | done: cbindgen-generated `pico_link_ui.h`, `PL_EVENT_ABI_VERSION=5`, `PL_COMMAND_ABI_VERSION=2` |
| | | | "D3 Screens: status … device list … pairing flow, settings" | all present: `render/hero.rs`, `app/screens/devices.rs`, `render/wizard.rs`, `app/screens/settings.rs` |
| | | | Live risk: "SPI clock is held at a deliberately conservative 1MHz, making a full 240x240 blit take ~1.008s" | 75 MHz, 38.6 ms (progress.md:298-299); damage-rect blit ~0.5 ms (design INDEX) |
| | | | Live risk: "No automated input path on the real target (`pico-link-d7k`)" | `PL_DEBUG_REMOTE` (CMake:124) |
| | | | "core cross-compiles for `thumbv8m.main-none-eabihf`" (line 192) | `eabi` |
| | | | "a provenance gate runs before M4 merges … every non-trivial C file we write carries a one-line provenance header" | 4 of 24 `firmware/src/*.c` have no header (`debug_remote.c`, `input.c`, `media_keys.c`, `volume.c`); no gate exists in tree |
| `.planning/progress.md` | 2026-08-31 (`e8569ef`) | No | "`main` @ `1e4734c`" | `2e37164` |
| | | | "Waiting on Andreas … `pico-link-nxf` … unproven by ear" | 3.5 weeks old |
| | | | "**Last updated:** 2026-08-28" (line 46, second header) | two "last updated" stamps in one file |
| | | | "One step left: `mv esp32-bluetooth-tx pico-link`" (line 83) | repo is `pico-link`; done |
| | | | "`PICO_SDK_PATH=/Users/andreas/pico-sdk` is REQUIRED" (line 387) | CLAUDE.md:541 and `hardware-debugger.md:92` say `/Users/andreas/.pico-sdk/sdk/2.1.1`; `apply-sdk-patches.sh:2` defaults to `~/.pico-sdk/sdk/2.1.1` |
| | | | "Open beads: `pico-link-zzq`, `cz0.4`, `d7k`, `gap`, `hfc`" | all but `hfc` resolved (see rows above) |
| | | | "Nothing added to CORE0's executor may block for >8s. A hardware watchdog is armed for 8s and fed every 3s" (line 416) | current watchdog is `watchdog_sup.c`, **observe-only** (`PL_WDT_OBSERVE_ONLY` ON, CMake:327) — never trips |
| `README.md` | 2026-08-27 (`90fa816`) | No | "**Status: early.** The repo currently holds the UI framework and its desktop emulator. Firmware integration is in progress." | firmware streams LDAC, has ABR, volume sync, fault strip, persistence |
| | | | §Building lists only `cargo build/test/run` | no firmware build steps at all (see F-docs-10) |
| `AGENTS.md` | 2026-08-26 (`c2de3f3`) | No | "`bd update <id> --status in_progress`" (top block) vs "`bd update <id> --claim`" (managed block) | contradictory within one file; see F-docs-12 |
| `.planning/decisions/INDEX.md` | 2026-09-23 | Partly | 2026-08-31 RenderCtx: "Accepted (designed, not yet implemented)" | `core/src/render/ctx.rs` added `a5e8f33` 2026-08-31; every `Widget` method takes `&RenderCtx` (`widget.rs:303-576`) |
| `.planning/design/INDEX.md` | 2026-09-24 | Partly | see F-docs-06 (seven rows wrong) | |
| `.planning/design/2026-08-28-on-device-ui.md` §21 E8 | 2026-09-01 | No | "`RenderCtx` (`pico-link-znb.10`, designed, not yet implemented)" | implemented 2026-08-31 |
| `.research/findings/INDEX.md` | 2026-08-26 | No | "(none yet)" | `.research/captures/2026-08-31-ldac-dwell-fix/` exists, unindexed |
| `.claude/agents/rust-embedded-supervisor.md:155,169` | 2026-09-02 | No | `thumbv8m.main-none-eabihf` (twice) | `eabi` |
| `.claude/agents/rust-embedded-supervisor.md:181-185` | | No | "There is no firmware crate in this repo yet — that's Epic C … migration design pending from the architect" | `ui-ffi/` (2.6k lines) and `firmware/` exist |
| `.claude/agents/tester.md:66` | 2026-09-02 | No | "once Epic C lands, the USBPods firmware fork (CMake/pico-sdk)" | Route B, never forked (ADR 2026-08-27 §Update) |
| `firmware/sdk-patches/README.md` §04 | 2026-09-23 | No | "ISO-OUT re-arm in TRUE ISR context (EXPERIMENT, default OFF)" | `CMakeLists.txt:131` `option(PL_USB_ISO_XFER_ISR … ON)`; `tusb_config.h:196` default `1` |
| `firmware/CMakeLists.txt` comments | 2026-09-24 | No | ":348 "core1 is never launched"; :419 "Core1 is STILL never launched"; :426 "nothing runs on core1 yet"; :540 "multicore_launch_core1() is NOT called anywhere yet; that is G3" | `a2dp.c:2556 multicore_launch_core1_with_stack(...)`, default ON since `0c643f1` |
| `firmware/CMakeLists.txt:430-434` | | No | `PICO_CORE1_STACK_SIZE=0x1000` justified by "`.stack1_dummy` … SCRATCH_X … 4KB" | core1 runs on `a2dp.c:2512 s_core1_stack[1024]` passed explicitly; the SDK define is now inert for the encoder thread |
| `core/Cargo.toml:15` | | No | "See: `.planning/decisions/2026-08-11-portability-boundary-and-workspace-split.md`" | file does not exist; nearest is `2026-08-11-ui-framework-reuse-vs-rewrite.md` |
| `firmware/src/st7789.c:1-2` | | Partly | "Ported from `firmware-spike/src/main.rs:285-410`" | true today; dangles if F-docs-17 deletes the spike |

### F-docs-02: There is no CI, and two instruction surfaces gate merges on it
- Severity: P1   Confidence: High   Effort: M   Tier: Sonnet
- Location: `.github/` (absent); `CLAUDE.md:93,108`; `.claude/skills/auto-run/SKILL.md:20`
- Evidence: `ls .github` → "No such file or directory". CLAUDE.md:93: "Orchestrator merges
  to `main` once CI passes and review is clean". auto-run step 4: "merge to `main` when CI
  is green". 139 merge commits exist on main.
- Why it matters: the rule is unsatisfiable, so it is silently skipped, and with it the one
  cheap mechanical gate (`cargo test`, host C tests, an `eabi` cross-compile of `ui-ffi`)
  that would have caught a broken `no_std` build before merge. Nothing in the tree proves
  `ui-ffi` still cross-compiles at `2e37164`; the last such proof is whatever machine last
  ran CMake.
- Fix sketch: add the workflow in Appendix A (host tests + clippy + fmt + host C tests +
  `cargo build -p ui-ffi --target thumbv8m.main-none-eabi` + cbindgen header generation).
  The full firmware build is a second, optional job that fetches pico-sdk 2.1.1 by tag and
  applies the patch script — everything it needs is already scripted. Then reword
  CLAUDE.md:93 to name the workflow.
- Verification: a PR against `main` shows the checks; `gh run list --limit 1` is green.
- Related: none (never filed; `progress.md` and beads never mention CI)

### F-docs-03: `roadmap.md` is the authority the vision skill executes against and it describes the August project
- Severity: P1   Confidence: High   Effort: M   Tier: Sonnet
- Location: `.planning/roadmap.md:3,89,137,143-150,180-183,205-211`
- Evidence: see the roadmap rows in the currency table. The sharpest one: line 143-146
  "**D1** … Rust owns SPI, DMA and the framebuffer directly now that Rust owns the binary"
  sits in the same file that says on line 215 "C owns `main()` … Rust is a staticlib". The
  "Live risks" list still carries the 1 MHz SPI clock (retired 2026-08-28, `progress.md:298`).
- Why it matters: `.claude/skills/vision-session/SKILL.md:20` — "read `.planning/roadmap.md`
  … so you're grounded in where things stand … Never re-litigate a decision already marked
  Accepted". A Vera session run today would plan M3 and D1–D4 as future work.
- Fix sketch: rewrite §Milestones as a table with a Status column (A ✅, B ✅, C ✅ M1–M5,
  D1–D3 ✅, D4 ⬜ *not recorded*, E ⬜), strike retired risks, add the post-MVP work that
  actually happened (volume sync epic 4v2, fault model 9eq2, ABR 7jol, core1 nli, cushion
  8pp1) as milestone F, and fix D1's text. Keep "Settled decisions".
- Verification: `grep -n 'IN FLIGHT\|in progress\|1MHz\|eabihf\|Rust owns SPI' .planning/roadmap.md` returns nothing.
- Related: `vision-session` skill; ADR 2026-08-27

### F-docs-04: `progress.md` stopped on 2026-08-31; 260 commits have no status record
- Severity: P1   Confidence: High   Effort: M   Tier: Sonnet
- Location: `.planning/progress.md:3-42` (last entry), `:46` (second "Last updated")
- Evidence: `git log -1 --format=%ad -- .planning/progress.md` → 2026-08-31. Commits per
  month: Aug 244, Sep 260. Six of the eight "Open beads" (lines 352-378) are resolved.
- Why it matters: `session-resume` trusts the handoff; `scribe.md` says it "keep[s] docs in
  sync with code" but no bead or hook triggers it. The September work (volume epic, fault
  strip, ABR, core1 flip, screensaver, live widgets, congestion cushion) exists only in
  merge-commit subjects and the design INDEX.
- Fix sketch: one new dated entry per merged epic since 09-01 (the merge subjects already
  contain the summaries), then move the pre-09-01 content under "## History" unchanged.
  Delete the duplicate "Last updated: 2026-08-28" line. Make `session-close` step 3 write
  the entry (it currently writes to a *memory* file outside the repo — SKILL.md:52-63).
- Verification: `head -5 .planning/progress.md` names `main`'s current hash; "Open beads"
  section matches `bd list --json --status open` (manual, needs `bd`).
- Related: `.claude/skills/session-close/SKILL.md`

### F-docs-05: `README.md` says the repo holds only the UI framework
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `README.md:15-17, 33-40`
- Evidence: "**Status: early.** The repo currently holds the UI framework and its desktop
  emulator. Firmware integration is in progress." Tree: `firmware/src` 24 C files incl.
  `a2dp.c` (2.5k lines), `codec_ldac.c`, `volume.c`, `fault.c`, `persist.c`.
- Why it matters: it is the first thing a new contributor or model reads; it also omits the
  firmware build entirely (see F-docs-10).
- Fix sketch: two paragraphs of status + a "Firmware build" section copied from F-docs-10's
  step list.
- Verification: `grep -c 'Status: early' README.md` = 0; README contains `apply-sdk-patches`.

### F-docs-06: `design/INDEX.md` marks seven merged designs "unbuilt" and one unbuilt design "Live"
- Severity: P1   Confidence: High   Effort: S   Tier: Haiku
- Location: `.planning/design/INDEX.md:21-24, 36, 39, 51`
- Evidence (INDEX status → merge that built it):
  ```
  volume-on-display        "Design of record, unbuilt (pico-link-4v2.6)"  -> 2c646a2 2026-09-07 merged VT6
  ldac-quality-selector    "unbuilt (pico-link-7jol.2)"                    -> afa341b 2026-09-07 QUALITY row + picker
  audio-fault-model        "unbuilt (pico-link-9eq2.3)"                    -> 15185f1, 5cb6048 2026-09-08 (firmware/src/fault.c)
  home-fault-strip (09-07) "unbuilt (pico-link-9eq2.2)"                    -> 5efba31 2026-09-08 (render/fault_glyph.rs, app/screens/why_page.rs)
  link-state-vs-discovery  "unbuilt (pico-link-88xs)"                      -> 3818d75 2026-09-08 (BtModel::discovering, LinkGlyph)
  core1-encoder-default    "does NOT flip PL_ENCODER_ON_CORE1's default"    -> 0c643f1 2026-09-23 flipped ON (decisions/INDEX already says so)
  composite-damage-and-paint-plan "Live"                                   -> `PaintPlan` appears nowhere in core/src; paint_key/damage_hint/damage_region_key it says it replaces are still the API (widget.rs:343-404). Design NOT built.
  ```
- Why it matters: this file is the router a model uses to answer "is X built?". Seven wrong
  answers out of 39 rows, in both directions.
- Fix sketch: add "Built: `<merge hash>`" to each row; for `composite-damage-and-paint-plan`
  write "Design of record, unbuilt (no bead)". Adopt the rule already used by the 09-23 docs
  ("Live, implemented on `bd-…`") for every row.
- Verification: for every row marked Live/implemented, `git log --oneline --grep=<bead>`
  is non-empty; `grep -rn PaintPlan core/src` is empty until it is built.

### F-docs-07: `decisions/INDEX.md` and the on-device-ui design still say `RenderCtx` is unimplemented
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `.planning/decisions/INDEX.md:14`; `.planning/decisions/2026-08-31-render-ctx-frame-scoped-clock.md:5`; `.planning/design/2026-08-28-on-device-ui.md:692` (E8)
- Evidence: "Accepted (designed, not yet implemented)". `core/src/render/ctx.rs` added in
  `a5e8f33` (2026-08-31); `widget.rs:303 fn measure(&self, constraints: Size, ctx: &RenderCtx)`,
  `:523 fn redraw_after(&self, _ctx: &RenderCtx)` — the exact signatures the ADR specifies.
- Why it matters: E8 is on the MVP Tier-1 list; a model auditing MVP completeness would
  count it missing.
- Fix sketch: status → "Accepted, implemented `a5e8f33`…" in all three places.
- Verification: `grep -rn 'not yet implemented' .planning/decisions/INDEX.md .planning/design/2026-08-28-on-device-ui.md` empty.

### F-docs-08: Three agent-facing docs name the wrong Rust target triple
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `CLAUDE.md:51`; `.planning/roadmap.md:192`; `.claude/agents/rust-embedded-supervisor.md:155,169`; (`progress.md:90` too, but `:274` corrects it)
- Evidence: all say `thumbv8m.main-none-eabihf`. `firmware/CMakeLists.txt:180`
  `set(UI_FFI_TARGET_TRIPLE thumbv8m.main-none-eabi)` with a 12-line comment: "Mixing eabihf
  Rust objects with this softfp C build fails link with 'uses VFP register arguments'".
- Why it matters: a supervisor verifying "does core still cross-compile" per its own agent
  file will `rustup target add …eabihf` and test the wrong ABI; the CMake path is unaffected
  but the human-run check is meaningless.
- Fix sketch: s/eabihf/eabi/ in the four places; add one clause "(not eabihf — softfp ABI,
  see firmware/CMakeLists.txt)".
- Verification: `grep -rn eabihf CLAUDE.md .planning .claude` empty.

### F-docs-09: Watchdog has been observe-only for 26 days; design step 7 was never executed and nothing tracks it
- Severity: P2 (P1 for the firmware seam)   Confidence: High   Effort: M   Tier: Sonnet
- Location: `firmware/CMakeLists.txt:320-333`; `.planning/design/2026-08-30-watchdog.md:322-335`
- Evidence: CMake: `option(PL_WDT_OBSERVE_ONLY "… never trips a reset" ON)` with comment
  "Rollout step 6 … first flash … until step 7 explicitly undefines this". Design step 7:
  "Undefine `PL_WDT_OBSERVE_ONLY` … prove the trip path with a deliberate fault-injection
  build … Acceptance is the breadcrumb". No `PL_DIAG_WDT_TEST` option exists (`grep -c
  WDT_TEST CMakeLists.txt` = 0). `progress.md:416` still describes the *previous*
  (embassy-era) watchdog as armed.
- Why it matters: every hang since 2026-08-30 that needed a physical BOOTSEL press is a hang
  the watchdog was designed to recover from and was configured not to. The design's own
  rollout has an unfinished step with no bead visible in the tree.
- Fix sketch: file the step-7 bead; add `PL_DIAG_WDT_TEST`; flip the default after the
  breadcrumb is proven on hardware. Until then, say so in `progress.md`.
- Verification: `cmake -LA build | grep PL_WDT_OBSERVE_ONLY` shows OFF; on-hardware
  breadcrumb (manual).
- Related: bead `pico-link-ufh`, design 2026-08-30-watchdog.md

### F-docs-10: A fresh clone cannot build the firmware from any single documented procedure
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `README.md` §Building; `CLAUDE.md:330-343, 528-541`; `firmware/CMakeLists.txt:189-210`
- Evidence: the steps are scattered and one is undocumented. What CMake actually needs
  (traced from `CMakeLists.txt`): (1) `PICO_SDK_PATH` to pico-sdk **2.1.1** (only stated as
  a Mac path, `CLAUDE.md:541`); (2) `tools/apply-sdk-patches.sh` run first or configure
  `FATAL_ERROR`s (CMake:31); (3) an ARM GNU toolchain on PATH or `PICO_TOOLCHAIN_PATH`
  (CLAUDE.md names a Mac path only); (4) `rustup` + `cargo` (CMake:193 adds the target
  itself); (5) **`cbindgen` on PATH** — CMake:204 invokes it, and it is mentioned in no
  instruction file (`grep -rn cbindgen CLAUDE.md README.md` → nothing); (6) `cmake -B
  build && cmake --build build`. `PICO_BOARD` is set in-file (CMake:4) so board selection is
  fine. The stock Pico 2 W "reference board" mentioned in CLAUDE.md:339 has no build path
  (no `-DPICO_BOARD=pico2_w` documented, and `st7789.c`/`input.c` pin maps are for the LCD
  hat regardless).
- Why it matters: the CI in Appendix A and any second machine both need this list; the
  cbindgen omission alone costs a configure failure with a non-obvious message.
- Fix sketch: a "Firmware build" section in README with the six steps and the
  Linux-generic form (`PICO_SDK_FETCH_FROM_GIT=1 PICO_SDK_FETCH_FROM_GIT_TAG=2.1.1`
  works with the vendored `pico_sdk_import.cmake:43-56`). Move the Mac paths to a
  machine-notes file (F-docs-14).
- Verification: on a clean Linux container, the README steps produce
  `build/pico_link.uf2` (Appendix A job `firmware`).

### F-docs-11: SDK patches 04–06 exist only inside the apply script; the `.patch` files and README disagree with CMake on defaults
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `firmware/sdk-patches/` (3 `.patch` files); `tools/apply-sdk-patches.sh` (embeds 01-06 as Python string replacements); `firmware/sdk-patches/README.md` §04; `firmware/CMakeLists.txt:66-119,131`
- Evidence: CMake enforces markers for six patches (`audiod_xfer_isr`, the dcd reorder
  string, `pl_usb_fifo_shortfall_bytes`). `ls firmware/sdk-patches/*.patch` → 01, 02, 03
  only. The `.patch` files are not what applies anything — the script does exact-string
  replacement, so the `.patch` files are documentation that can drift. README §04 header:
  "(EXPERIMENT, default OFF)"; CMake:131: `option(PL_USB_ISO_XFER_ISR … ON)`; `tusb_config.h:196`
  `#define PL_USB_ISO_XFER_ISR 1`.
- Why it matters: a reviewer diffing "what did we change in the SDK" reads three files and
  misses three patches; the README tells a model the ISR path is an experiment that is off.
- Fix sketch: either generate `04-06.patch` (`diff -u` of stock vs patched SDK file) or
  delete the three existing `.patch` files and state "the script is the source of truth".
  Fix the §04 header to "default ON since `c005a84`/`e29b899` 2026-09-01".
- Verification: `ls firmware/sdk-patches/*.patch | wc -l` equals the number of `## NN`
  headings in README; `grep -n 'default OFF' firmware/sdk-patches/README.md` empty.

### F-docs-12: `AGENTS.md` contradicts itself on pushing and on the claim command
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `AGENTS.md:13, 17-38` vs `:56-64, 80-88`
- Evidence: top block (hand-written, 2026-08-26): "Work is NOT complete until `git push`
  succeeds … NEVER stop before pushing … `bd update <id> --status in_progress`". Managed
  block: "Conservative (default): … Do not run git commits, git pushes, or Dolt remote sync
  unless explicitly asked … `bd update <id> --claim`". `session-close` SKILL.md:93: "Do not
  push without explicit authority."
- Why it matters: a Codex/other-vendor agent reads `AGENTS.md`, not `CLAUDE.md`; it gets
  both "you must push" and "do not push" in one file.
- Fix sketch: delete the hand-written top block (lines 1-38); the managed block plus a
  one-line pointer to `CLAUDE.md` is sufficient.
- Verification: `grep -c 'MANDATORY' AGENTS.md` = 0.

### F-docs-13: Process rules in `CLAUDE.md` are stricter than the process the history shows
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `CLAUDE.md:207 ("Every task goes through beads. No exceptions"), :110-126 (Quick Fix on a feature branch only), :93`
- Evidence: `git log --first-parent --no-merges main | wc -l` = **73** commits that landed
  on main outside a merge. Most are `docs(design)`/`docs(adr)` (the de-facto convention:
  design docs are committed straight to main by the orchestrator, allowed by
  `enforce-branch-before-edit.sh:9-10` for `.planning/` and `.research/`). But ~10 are code:
  `fe8ce44 fix(fault)` (09-08), `c005a84`+`e29b899` (USB ISR default, 09-01), `eb1c4db
  feat(firmware): watchdog` (08-30), `edd2a99 feat(ui)`, `306904e fix(render)`, `8b10514
  fix(emulator)`, `68a4a34 fix(ui-ffi)`, `67f1d94 test(core)`. 203 of 504 commit subjects
  carry no bead id (many are the S-series refactors whose bead is on the merge commit —
  acceptable, but a `git log --grep=<bead>` audit misses them).
- Why it matters: not a judgement on the practice — a cheaper model reading "no exceptions"
  will refuse legitimate doc commits, or, worse, learn from history that the rule is
  decorative. Document the real rule.
- Fix sketch: in §Workflow add: "Docs under `.planning/` and `.research/` are committed
  directly to `main` by the orchestrator. Code reaches `main` only by merge from a
  `bd-*` branch (fast-forward allowed) whose bead id is in the merge subject." Put the
  bead id in every commit subject, not only the merge.
- Verification: future `git log --first-parent --no-merges main -- core firmware ui-ffi emulator` stays empty.

### F-docs-14: `CLAUDE.md` carries ~150 lines of one-machine notes inside the project instruction set
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `CLAUDE.md:305-328, 386-541`
- Evidence: 14 lines reference `/Users/andreas`, `/Applications/ArmGNUToolchain`,
  `/Volumes/RP2350`, Homebrew or "this Mac". Items: the kernel-panic tty rule, `timeout`
  absent, SSH port 22 blocked, the auto-mode classifier, `/opt/homebrew/bin/bd`
  (`memory-capture.sh:369` hard-codes the same). The whole file is 647 lines and is loaded
  into every context.
- Why it matters: (a) tokens on every turn; (b) a Linux CI runner or a second developer
  hits rules that are false for them ("`timeout` does not exist" — it does here; the
  charter says so); (c) the kernel-panic rule is *important* and is buried at line 388 among
  beads-daemon trivia.
- Assessment by category (task item 5):
  - **(a) still true and generic** — Tech Stack (except triple), Repo layout (incomplete),
    Team table, Orchestrator Autonomy, Investigation-before-delegation, Beads commands,
    Planning conventions, `PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1` (verified `CMakeLists.txt:399`),
    the `cdc_reader.py` rule, "one tool on the bus", pull-ups not pull-downs, the
    review-against-merge-base rule.
  - **(b) machine-specific → move to `.planning/machine-notes-andreas-mac.md`** — toolchain
    and SDK paths, kernel-panic tty warning (keep a one-line pointer in CLAUDE.md), no
    `timeout`, SSH 443, Homebrew bd path, screencapture permission, beads-daemon recovery
    recipes, worktree `.cargo/config.toml` inheritance, auto-mode classifier.
  - **(c) contradicted by the code** — `eabihf`; "Build produces a `.uf2`; flash by holding
    BOOTSEL" vs the CDC `--bootsel` default 25 lines earlier; §Current State (all of it).
  - **(d) rules the history shows are not followed** — "CI green" (no CI); "No exceptions"
    (73 direct commits); "`.research/findings/` … indexed" (index empty, captures unindexed);
    "provenance gate before M4 merges" (none in tree, 4 files lack headers).
- Fix sketch: split into `CLAUDE.md` (≤250 lines: identity, rules, pointers) and the
  machine-notes file; keep the DTR flag and the tty rule as one-liners with links.
- Verification: `wc -l CLAUDE.md` < 300; `grep -c '/Users/' CLAUDE.md` = 0.

### F-docs-15: Hooks are not registered by any tracked file, and two of them hard-block without `bd`
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `.gitignore:29` (`.claude/settings.json`); `.claude/hooks/validate-completion.sh:47-52`; `.claude/hooks/memory-capture.sh:368-369`
- Evidence: `git ls-files .claude | grep settings` → nothing; the 14 hook scripts exist but
  the file that binds them to events is gitignored, so a fresh clone runs none of them (the
  Codex side does register `bd codex-hook …` in `.codex/hooks.json`). CLAUDE.md:73-81
  ("Hard gates are enforced by hooks") and `task-manager.md:15-19` assume they run.
  `validate-completion.sh` blocks a supervisor's stop unless its transcript contains a
  `bd comments` call and the branch exists on `origin` (`ls-remote`) — on a machine without
  `bd`, or offline, every supervisor is blocked at exit with no fallback.
  `session-start.sh:503-505` degrades gracefully; `enforce-sequential-dispatch.sh:211`
  silently passes when `bd show` fails (a closed bead becomes dispatchable).
- Why it matters: the process the docs describe is enforced on exactly one laptop; on a
  second machine it is advisory. The docs should say which.
- Fix sketch: commit a `.claude/settings.json` template (`settings.example.json`) with the
  hook bindings and a README line; make `validate-completion.sh` `approve` when `command -v
  bd` fails, and print why.
- Verification: `git ls-files .claude/settings.example.json`; run the hook with `PATH=/bin`
  and a fake transcript → `{"decision":"approve"}`.

### F-docs-16: Agent definitions still carry web-app boilerplate that contradicts this project
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `.claude/agents/code-reviewer.md:61-66,139,166,196`; `.claude/agents/discovery.md` (475 lines); `.claude/ui-constraints.md` (76 lines); `.claude/beads-workflow-injection.md:16-27, 112`; `rust-embedded-supervisor.md:46-50, 143`; `tester.md:73`; `scout.md:66-70`
- Evidence: code-reviewer's mandatory Phase 0 example is `curl localhost:3008/api/endpoint`
  and "Cross-layer consistency (DB → API → Frontend)". `discovery.md` detects
  `package.json + react/next` and installs a `react-best-practices` skill. `ui-constraints.md`
  is Tailwind/`motion/react`/WCAG rules for a 240x240 ST7789. `beads-workflow-injection.md`
  and the supervisor POST to `http://localhost:3008/api/git/worktree` (a kanban UI not in
  this repo) and ban "Merging your own branch (user merges via PR)" — CLAUDE.md:93 says the
  orchestrator merges, and there are 139 merges and 0 PRs. `scout.md:68` example is
  `Glob("**/*.tsx")`. `progress.md:276-280` records a sweep that "returns zero hits" for
  old-product terms — it did not sweep for these.
- Why it matters: `code-reviewer` is declared the quality gate (CLAUDE.md:87-90); its
  primary instruction ("re-run every DEMO block … `curl`") cannot be followed here, so a
  Sonnet reviewer either invents a DEMO or rubber-stamps. `pico-link-hfc` ("remaining
  .claude boilerplate", P4) is the open bead for this and is the one CLAUDE.md-listed bead
  that is genuinely still open.
- Fix sketch: delete `discovery.md` and `ui-constraints.md`; replace code-reviewer Phase 0
  with the project's real demo forms (`cargo test` counts, a headless PNG, a CDC capture, a
  host C test line); remove the localhost:3008 branch and the "user merges via PR" line;
  make the injection file match CLAUDE.md.
- Verification: `grep -rn 'localhost:3008\|React\|Tailwind\|via PR' .claude` empty.
- Related: bead `pico-link-hfc`

### F-docs-17: `firmware-spike/` — delete
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku
- Location: `firmware-spike/` (152 KB, 1,087 lines of Rust/C, own `Cargo.lock` and
  `rust-toolchain.toml`, last touched `bbe18a7` 2026-08-27)
- Evidence: not a workspace member (`Cargo.toml:3`), not built by anything, referenced only
  by two ADRs and `progress.md` as history, and by `st7789.c:1-2` as the port source. ADR
  2026-08-27 §Consequences: "The embassy async executor and embassy-usb CDC console are
  retired". Its `rust-toolchain.toml` and `.cargo/config.toml` are a second, divergent
  toolchain pin inside the repo.
- Why it matters: it is the only Rust in the tree that depends on `embassy-rp`/`cortex-m`,
  which a model grepping "how is the RP2350 initialised" will find first.
- Fix sketch: `git tag firmware-spike-final bbe18a7`, `git rm -r firmware-spike`, change
  `st7789.c:1-2` to cite the tag. The ADRs already say "superseded".
- Verification: `test ! -d firmware-spike && git tag | grep firmware-spike-final`.

### F-docs-18: Reproducibility pins are partial
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `rust-toolchain.toml` (`channel = "stable"`); `firmware/pico_sdk_import.cmake:48-51`; `firmware/CMakeLists.txt` (no version assert); `firmware/vendor/libldac/PROVENANCE.md`
- Evidence (task item 7): `Cargo.lock` ✅ committed. Rust toolchain ❌ floats on `stable`
  (crates say `rust-version = "1.77"` as a floor only). pico-sdk version ❌ pinned by
  *directory name convention* (`~/.pico-sdk/sdk/2.1.1`) and by the patch script's exact
  stock strings (which will refuse a different TinyUSB — a good accidental pin); with
  `PICO_SDK_FETCH_FROM_GIT=1` and no tag the import file fetches **`master`**. BTstack and
  TinyUSB ❌ float with whatever SDK is at `PICO_SDK_PATH` (TinyUSB 0.18.0 is implied by
  `sdk-patches/README.md` §04). libldac ✅ vendored at a named AOSP commit. ARM toolchain ❌
  unpinned (CLAUDE.md names 15.2.rel1 by path only).
- Why it matters: "it built on the laptop" is the only reproducibility guarantee for the
  firmware; a `stable` bump can change `no_std` behaviour silently.
- Fix sketch: `rust-toolchain.toml` `channel = "1.xx"` (whatever last built the `.uf2`);
  `set(PICO_SDK_FETCH_FROM_GIT_TAG 2.1.1)` before the import and
  `if(NOT PICO_SDK_VERSION_STRING VERSION_EQUAL 2.1.1) message(FATAL_ERROR …)` after
  `pico_sdk_init()`; state the toolchain version in README.
- Verification: `cmake -B /tmp/b -S firmware` against an SDK 2.2.x dir fails with the version
  message; `rustup show` in-tree prints the pinned version.

### F-docs-19: No `LICENSE`/`NOTICE` for the project's own code; third-party terms are only described in prose
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet (needs Andreas's licence choice)
- Location: repo root (no `LICENSE*`, no `NOTICE*`); `README.md:53-60`; `CLAUDE.md:42-45`
- Evidence (task item 8): `find . -maxdepth 4 -iname 'LICENSE*'` → only
  `firmware/vendor/libldac/LICENSE`. README: "Bluetooth is BlueKitchen's BTstack as
  distributed with the Pico SDK" — that is a distribution channel, not a licence term.
  The cyw43-driver claim (CLAUDE.md:42, roadmap:230) is *consistent* with what is vendored:
  nothing — cyw43-driver, BTstack and TinyUSB all come from `PICO_SDK_PATH`, so `LICENSE.RP`
  applies to the SDK copy and nothing in this tree re-licenses it. USBPods (GPL-3) is not in
  the tree (`grep -rli usbpods firmware/src | wc -l` finds only comments naming it as a
  reference), consistent with ADR 2026-08-27 §Update.
- Why it matters: the ADR says "we keep a free choice of licence for our own code" — a
  choice that has not been made, so the repo is currently all-rights-reserved by default.
  A `NOTICE` listing pico-sdk (BSD-3), TinyUSB (MIT), BTstack (Raspberry Pi Pico licence
  grant), cyw43-driver (LICENSE.RP), libldac (Apache-2.0) is expected by Apache-2.0 §4(d)
  for libldac's NOTICE anyway once binaries are distributed.
- Fix sketch: Andreas picks a licence (§6 Q1); add `LICENSE` and `THIRD_PARTY_NOTICES.md`.
- Verification: files exist; README links them.

### F-docs-20: Verification process gaps — no "run before merge" list, fmt unenforced, clippy not pedantic-clean, one host test unbuildable from a clean tree
- Severity: P2   Confidence: High   Effort: M   Tier: Sonnet
- Location: `CLAUDE.md` (no such list); `.claude/agents/rust-embedded-supervisor.md:203` ("clippy::pedantic compliance — resolve warnings before completing"); `firmware/tests/*.c` headers; `firmware/tests/test_paired_device_upserted_ldac_quality_echo.c:46`
- Evidence (task item 9): there is no documented pre-merge command list; the code-reviewer
  agent has none either. `cargo fmt --all --check` → 17,295-line diff. `cargo clippy` → 7
  warnings at default lint level (pedantic would be more). The eight host C tests each
  document their own `cc` line ("no CMake target — same convention as the other
  firmware/tests/test_*.c files"); 7/8 pass here; the 8th needs `firmware/include/pico_link_ui.h`
  which only a CMake configure (or a manual `cbindgen` run) produces; `test_pcm_ring_cross_core.c`
  needs a hand-made `__dmb` stub in `/tmp/hostinc`. No script runs them all.
- Why it matters: "Tests: pass" in a supervisor's report currently means `cargo test`; the
  C tests, the cross-compile and the header generation are unexercised unless someone
  remembers.
- Fix sketch: `tools/host-tests.sh` that generates the header with `cbindgen --config
  ui-ffi/cbindgen.toml --crate ui-ffi --output /tmp/inc/pico_link_ui.h`, writes the `__dmb`
  stub, and runs all eight; a `## Before merge` block in CLAUDE.md: `cargo test --workspace`,
  `cargo clippy --workspace --all-targets -D warnings` (after fixing the 7), `cargo fmt
  --check` (after one formatting commit), `tools/host-tests.sh`, `cargo build -p ui-ffi
  --release --target thumbv8m.main-none-eabi`. Wire the same list into Appendix A.
- Verification: `tools/host-tests.sh` exits 0 on a clean clone; CI green.

### F-docs-21: Batch of P3 nits
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku
- Location / evidence / fix, one line each:
  - `core/Cargo.toml:15` cites `2026-08-11-portability-boundary-and-workspace-split.md`, which does not exist → point at `2026-08-11-ui-framework-reuse-vs-rewrite.md` or drop.
  - `firmware/src/{debug_remote,input,media_keys,volume}.c` have no provenance/module header while roadmap:243-244 promises one on "every non-trivial C file" → add the one-liner.
  - `.research/findings/INDEX.md` "(none yet)" while `.research/captures/2026-08-31-ldac-dwell-fix/` exists → index it (or move captures under findings per CLAUDE.md:297-299).
  - `home-screenshots/` (10 PNG) and `wizard-screenshots/` (19 PNG) at repo root are outputs of `core/examples/*_screenshots.rs`, not fixtures (`fixtures/` is) → gitignore or move under `fixtures/`.
  - Two different `PICO_SDK_PATH` values in agent-facing docs (`progress.md:387` vs `CLAUDE.md:541`/`hardware-debugger.md:92`/`apply-sdk-patches.sh:2`) → keep one.
  - `progress.md` has two "Last updated" stamps (`:5` implied, `:46` explicit 08-28).
  - `CLAUDE.md:293-301` says research findings go in `.research/findings/`; nothing has ever gone there in 504 commits → either use it or remove the convention.
  - `.claude/agents/hardware-debugger.md:4` `model: claude-opus-5` while every other agent uses the alias form (`opus`/`sonnet`/`haiku`) → normalise.
  - `firmware/CMakeLists.txt:396-398` comment "lets `picotool reboot -f -u` reset a running board … for free" contradicts CLAUDE.md:315-317 ("both STALL on this Mac") → point at `cdc_sender.py --bootsel`.
  - `.beads/README.md` is the stock beads marketing README ("moves at the speed of thought ⚡") → delete; `bd prime` covers it.
- Verification: each is a grep.

---

### ADR conformance (task item 2)

| ADR | Decision (one line) | Honoured? | Evidence |
|---|---|---|---|
| 2026-08-11 UI framework | Chrome = fixed named regions + linear widget stack; no layout engine | **Yes** | `core/src/render/chrome.rs` (`compute_chrome`), `widget.rs` linear `Widget` trait, `rail.rs`; no flex/grid code in `core/src/render/` |
| 2026-08-26 Rust owns binary | Rust `main`, C libs linked in | **Superseded** (INDEX says so) | `firmware/src/main.c:92 int main(void)`; no `cortex-m-rt` in `ui-ffi/Cargo.toml` |
| 2026-08-27 TinyUSB owns USB | TinyUSB CDC+UAC2+reset composite; embassy-usb unwound | **Yes** | `firmware/src/usb_descriptors.c`, `usb_audio.c`, `usb_reset.c`; `CMakeLists.txt:567 tinyusb_device`; no embassy outside `firmware-spike/` |
| 2026-08-27 C-first | pico-sdk owns `main()`/runtime; `core` is a `no_std` staticlib over a narrow FFI; Route B (no USBPods code) | **Yes** | `main.c:92`; `ui-ffi/Cargo.toml:19 crate-type=["staticlib"]`; 11 `extern "C"` entry points (`lib.rs:358-2581`); `vendor/libldac/PROVENANCE.md` "no byte of USBPods"; provenance gate promised (roadmap:244) **not** in tree — 4 files lack headers (F-docs-21) |
| 2026-08-31 RenderCtx | Frame-scoped `RenderCtx` threaded through `measure/render/chrome_contribution/redraw_after`, not a Clock object | **Yes, implemented** | `core/src/render/ctx.rs`; `widget.rs:303,319,439,523,576`; INDEX status wrong (F-docs-07) |
| 2026-09-02 core1 allocation | No display on core1; core1 = LDAC only, C, no heap; damage-rect blit is the ceiling-raiser | **Yes** | `a2dp.c:2366-2556` core1 entry is C encoder loop; no Rust symbol on core1 path; `pl_ui_render_ex` damage rect (`lib.rs:831`, `main.c:741`); `3uq` blit split not merged (grep `blit_split` empty) — as the ADR demanded |
| 2026-09-03 LDAC on core1 | Encoder+ring fill on core1 behind `PL_ENCODER_ON_CORE1`; OFF kept as fallback | **Yes** | `CMakeLists.txt:359 option(... ON)`; `a2dp.c:2556 multicore_launch_core1_with_stack`; OFF path compiles (`#if` sites in a2dp.c); ADR header records the 09-23 flip. Stale CMake *comments* claim core1 never launches (F-docs-01 table) |

INDEX marks the superseded/amended ones correctly. One structural note: the 2026-09-03
"ADR" is 1,012 lines and contains a bead breakdown (§8), measured results (§11.0) and a
"what I got wrong" section — it is a design + lab notebook wearing an ADR filename. That
is fine as a record, but the *decision* is three lines and should be liftable; consider
the rule "ADR ≤ 2 pages, evidence in `.research/`" going forward.

### Design docs vs code (task item 3) — 14 sampled, mechanism docs prioritised

| Doc | Verdict | Evidence |
|---|---|---|
| 2026-09-24 app-rs-split | **As written** | S0–S7 commits `1aa5cd7`…`225db04`; tree = design §"Target tree" (`core/src/app/{events,fault,fold,inspect,invariants,model,refresh,screen_id,ui_state,test_support,tests}.rs` + `screens/`) |
| 2026-09-24 congestion-cushion | **Partly (S1 only)** | `a2dp.c:794-797` stall histogram, `:2147 pl_a2dp_resync_decide` hold policy, merge `9f8b5f3`; S3 `PL:S:1` and S4 `AudioSettings`/`SET_AUDIO_SETTINGS` absent (`persist.c` has only `PL:S:0`; grep in `lib.rs`/`model.rs` empty). INDEX "Live" is right for S1 only |
| 2026-09-23 core1-encoder-default | **As written, then exceeded** | objcopy `.time_critical` rename `CMakeLists.txt:245-263` + `cmake/check_ldac_not_in_flash.cmake`; doc says "does NOT flip default" — flipped later by `0c643f1` (decisions INDEX notes it; design INDEX does not) |
| 2026-09-23 core1-resync-trim | **As written** | `a2dp.c:2147 _decide` / `:2193 _apply` (core1) / `_complete`; cancel-at-arm comment `:766-772` |
| 2026-09-23 on-arm-ring-collapse | **As written** | `aa8590d`; `samples_owed` clamp comments `a2dp.c:234-252`; `starved_us`/edge-counted underruns present |
| 2026-09-23 usb-out-fifo-loss-off-build | **As written** | `usb_pump.c:143 pl_usb_fifo_shortfall_bytes`, `:251-263` drain outside `pl_usb_mutex`; patch 06 marker enforced `CMakeLists.txt:104-118` |
| 2026-09-07 composite-damage-and-paint-plan | **Not implemented** (INDEX says Live) | `PaintPlan` absent; `paint_key/damage_hint/damage_region_key` still the trait (`widget.rs:343-404`) |
| 2026-09-07 audio-fault-model | **As written** | `firmware/src/fault.c`, `test_fault_evaluator.c` (passes), event tag 15 (`lib.rs` `PlEventTag`), `FaultLog` in `core/src/app/fault.rs` |
| 2026-09-07 home-fault-strip (v2) | **As written** | `render/fault_glyph.rs`, `app/screens/why_page.rs`, X binding `d6781c9`; INDEX "unbuilt" wrong |
| 2026-09-08 link-state-vs-discovery | **As written** | `BtModel::discovering` (`app/model.rs`), `LinkGlyph` (`render/widget.rs`), `PlDiscoveryState` in cbindgen export list, `PL_EVENT_ABI_VERSION` still 5 (`lib.rs:1902`) as the doc promised |
| 2026-09-02 dirty-gate-across-the-ffi-seam | **As written** | `pl_ui_dirty` (`lib.rs:726`), `PL_FORCED_REPAINT_MS=1000` (`CMakeLists.txt:591`) citing the doc's §5 |
| 2026-09-01 idle-policy-across-the-ffi-seam | **As written** | `pl_ui_display_power` (`lib.rs:664`), backlight PWM `main.c:693-694`; the doc's own finding "the firmware does not run `core/src/run.rs`" still true (`main.c` owns the loop) |
| 2026-08-30 watchdog | **Implemented differently: stuck at rollout step 6** | `watchdog_sup.c`; `PL_WDT_OBSERVE_ONLY` still ON (F-docs-09) |
| 2026-08-30 ldac | **As written** | `codec_ldac.c`, vendored libldac, `c01798f` 2026-08-31; ABR later added via `ldacBT_alter_eqmid_priority` (`codec_ldac.c:256`) per the 09-07 ABR doc, own controller (libldac `abr/` deliberately not vendored, PROVENANCE.md) |
| 2026-08-28 on-device-ui §21 Tier 1 | **12/13 verified built** | E1 `CancelScan` `lib.rs:2303`; E2 `render/rail.rs`; E3 `theme.rs:184 fn hero`; E4 `hero.rs`; E5 `wizard.rs`; E7 `HomeFace`; E8 `ctx.rs`; E9 `class_of_device` `lib.rs:1168`; E10 icons (`message.rs`/`list.rs`); E11 `fields.rs`; E12 `list_identity_png` test; E13 `HidLinkState` gone. **E6** (disabled-but-focusable `MenuItem` with reason) not found by grep — unverified |

INDEX vs directory: all 39 files are indexed and all indexed files exist (checked by
listing). Docs that are decisions wearing design clothes: `2026-09-02-a-button-label-rule.md`
("Status: Ruling"), `2026-09-02-field-list-widget-ruling.md`, `2026-08-30-cancel-connect.md`
(Context/Decision/Rationale/Alternatives/Consequences — the ADR template) → these three
belong in `decisions/`. The reverse case: `2026-09-03-ldac-encoder-on-core1.md` is a design
+ measurement log filed as an ADR (see above).

### Roadmap vs progress vs MVP (task item 4)

Roadmap milestones A–D are all marked "in progress"/"in flight"/unstated; progress.md says
A ✅ B ✅ C-M1a/M1b/M2 ✅ and stops at 2026-08-31 with M3 "still unmerged" — but the tree
has M3 (`91c2432`, 08-29), M4 SBC (`6edc201`, 08-29, commit subject: "the MVP"), M4 LDAC
(`c01798f`, 08-31), M5 persistence (`persist.c`), and every D-item. Neither doc says so.

**MVP ("pair a fresh set of headphones and see the live codec and bitrate on screen,
driven entirely by the buttons, with no serial console") — ruling: PARTLY / likely yes,
unproven in the record.**
- Yes, in code: on-device scan→connect wizard (`render/wizard.rs`, `WizardPhase`), scan /
  connect / cancel commands over the FFI, BTstack Classic with `NVM_NUM_LINK_KEYS 8`
  (`btstack_config.h:66`), live codec word + bitrate on the Home hero (`edd2a99`,
  `render/hero.rs`), LDAC negotiated and listenable (`progress.md:3-11`), `PL_DEBUG_REMOTE`
  OFF by default so the console is not required (`CMakeLists.txt:124`), 12/13 Tier-1 items.
- Not recorded anywhere: roadmap **D4** "On-hardware acceptance: pair fresh headphones
  using only the screen". `progress.md` 2026-08-31 records Andreas *listening*; the
  captures record a WH-1000XM3 streaming; no doc, bead comment in tree, or commit says the
  pairing was done screen-only from a cold bond store. That is the one sentence the MVP
  needs and it is a hardware fact I cannot establish here.

---

## 5. Test coverage (of the docs/build/process seam)

Covered: host Rust (578 tests), 8 standalone host C tests (each self-documenting), the
SDK-patch presence (configure-time), libldac-in-SRAM (post-link `nm` check), cbindgen
header freshness (build step).

Not covered, cheap to add (Haiku/Sonnet):
- A `tools/host-tests.sh` running all eight C tests (F-docs-20).
- A doc-link check: every `` `…/….md` `` path mentioned in `.planning/**`, `CLAUDE.md`,
  `core/**/Cargo.toml` exists (would have caught `core/Cargo.toml:15` and the INDEX drift).
  One `grep -o` + `test -f` loop.
- A "status hash" check: `CLAUDE.md`/`progress.md` name a commit reachable from `main`
  (`git merge-base --is-ancestor`).
- `cargo build -p ui-ffi --release --target thumbv8m.main-none-eabi` on every push
  (Appendix A) — the single most valuable missing check; it needs no SDK.
- A `.patch`-vs-script consistency test: apply the script to a pristine pico-sdk 2.1.1 in
  CI and `git diff --stat` the SDK; the diff's file list must equal the README's headings.

## 6. Open questions for Andreas

1. **Licence for our own code** (F-docs-19). ADR 2026-08-27 reserved the choice; nothing
   in the tree makes it. MIT/Apache-2.0 dual keeps every current dependency compatible.
2. **Was D4 ever done?** Pair fresh headphones, cold bond store, screen only, no console.
   One line in `progress.md` settles the MVP question. If not: it is the next hardware task.
3. **Watchdog step 7** (F-docs-09): flip `PL_WDT_OBSERVE_ONLY` off after a breadcrumb
   test, or record why it stays observe-only.
4. **Where should "status" live?** One file, updated at session close, is my
   recommendation; the alternative (keep four) has failed for four weeks.

## 7. Unverifiable here

- Whether `ui-ffi` cross-compiles and the firmware links at `2e37164` (no ARM toolchain,
  no `PICO_SDK_PATH`). Last evidence is `d071e5c`'s merge on 2026-09-24 claiming a build.
- Whether `tools/apply-sdk-patches.sh` applies cleanly to a pristine pico-sdk 2.1.1
  (needs the SDK; the script's exact-string matching makes this a yes/no CI check).
- E6 (disabled-but-focusable `MenuItem` with reason) — not located by grep; needs a read
  of `menu.rs`/`device_page.rs` by the render/app reviewer.
- D4 on-hardware acceptance (§6 Q2).
- Any claim in the `.beads` board (not in the tree; `bd` absent here). CLAUDE.md's list of
  "open beads that matter" was audited against git only.

---

## Appendix A — Proposed minimal CI (not written to `.github/`)

Job 1 needs nothing but Rust and a C compiler and should be required on every PR. Job 2
fetches pico-sdk 2.1.1 by tag through the repo's own `pico_sdk_import.cmake`, applies the
vendored patches, and does the full cross-build; it needs the ARM toolchain (~5 min) and
can start as `continue-on-error: true` until it is proven on a runner.

```yaml
name: ci
on:
  push: { branches: [main] }
  pull_request:

jobs:
  host:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable        # replace with the pinned version from rust-toolchain.toml
        with:
          components: clippy, rustfmt
          targets: thumbv8m.main-none-eabi
      - uses: Swatinem/rust-cache@v2
      - name: Host tests
        run: cargo test --workspace
      - name: Clippy (fails on the 7 existing warnings until they are fixed)
        run: cargo clippy --workspace --all-targets -- -D warnings
      - name: Format (fails until one formatting commit lands)
        run: cargo fmt --all --check
      - name: Cross-compile the FFI staticlib (no SDK needed)
        run: cargo build -p ui-ffi --release --target thumbv8m.main-none-eabi
      - name: Generate the FFI header
        run: |
          cargo install cbindgen --locked
          mkdir -p firmware/include
          (cd ui-ffi && cbindgen --config cbindgen.toml --crate ui-ffi --output ../firmware/include/pico_link_ui.h)
      - name: Host C tests (the eight standalone binaries)
        run: |
          set -e
          mkdir -p /tmp/hostinc/hardware
          printf 'static inline void __dmb(void) { __sync_synchronize(); }\n' > /tmp/hostinc/hardware/sync.h
          cc -std=c11 -Wall -Wextra firmware/tests/test_a2dp_priming_cushion_frames_per_packet.c -o /tmp/t1 && /tmp/t1
          cc -std=c11 -Wall -Wextra firmware/tests/test_a2dp_tx_ring_count.c -o /tmp/t2 && /tmp/t2
          cc -std=c11 -Wall -Wextra -I firmware/src firmware/tests/test_codec_id_stability.c firmware/tests/stub_codec_rows.c firmware/src/codec_table.c -o /tmp/t3 && /tmp/t3
          cc -std=c11 -Wall -Wextra firmware/tests/test_fault_evaluator.c -o /tmp/t4 && /tmp/t4
          cc -std=c11 -Wall -Wextra firmware/tests/test_ldac_abr_controller.c -o /tmp/t5 && /tmp/t5
          cc -std=c11 -Wall -Wextra firmware/tests/test_ldac_frames_per_packet.c -o /tmp/t6 && /tmp/t6
          cc -std=c11 -Wall -Wextra -I firmware/include firmware/tests/test_paired_device_upserted_ldac_quality_echo.c -o /tmp/t7 && /tmp/t7
          cc -std=c11 -Wall -Wextra -I firmware/src -I /tmp/hostinc firmware/tests/test_pcm_ring_cross_core.c firmware/src/pcm_ring.c -o /tmp/t8 && /tmp/t8
      - name: Doc links resolve
        run: |
          set -e
          grep -rhoE '\.planning/(decisions|design)/[0-9]{4}-[0-9]{2}-[0-9]{2}-[a-z0-9-]+\.md' CLAUDE.md README.md .planning core/Cargo.toml .claude \
            | sort -u | while read -r p; do test -f "$p" || { echo "dangling: $p"; exit 1; }; done

  firmware:
    runs-on: ubuntu-latest
    continue-on-error: true            # flip to false once it has passed once on a runner
    env:
      PICO_SDK_FETCH_FROM_GIT: 1
      PICO_SDK_FETCH_FROM_GIT_TAG: 2.1.1
      PICO_SDK_FETCH_FROM_GIT_PATH: ${{ github.workspace }}/.sdk
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: thumbv8m.main-none-eabi }
      - uses: carlosperate/arm-none-eabi-gcc-action@v1
        with: { release: '13.2.Rel1' }     # any release with newlib specs; pin what the laptop uses
      - run: sudo apt-get install -y cmake ninja-build && cargo install cbindgen --locked
      - name: Fetch pico-sdk 2.1.1 (submodules included) and apply the vendored patches
        run: |
          git clone --depth 1 --branch 2.1.1 --recurse-submodules https://github.com/raspberrypi/pico-sdk .sdk/pico-sdk
          PICO_SDK_PATH=$PWD/.sdk/pico-sdk tools/apply-sdk-patches.sh
      - name: Configure + build
        run: |
          export PICO_SDK_PATH=$PWD/.sdk/pico-sdk
          cmake -S firmware -B build -G Ninja
          cmake --build build
          ls -l build/pico_link.uf2
      - uses: actions/upload-artifact@v4
        with: { name: pico_link.uf2, path: build/pico_link.uf2 }
```

Notes for whoever lands it: (1) `pico_sdk_import.cmake` already honours the three
`PICO_SDK_FETCH_FROM_GIT*` env vars, but its FetchContent path may not init submodules
(cyw43-driver, btstack, tinyusb) — the explicit clone above is the safe form; (2) the patch
script is the marker source, so the `firmware` job doubles as the "patches apply to
pristine 2.1.1" test; (3) the `check_ldac_not_in_flash.cmake` post-link step runs in the
build and is a real gate.

## Appendix B — Doc-hygiene checklist for `session-close`

Add these as step 6 of `.claude/skills/session-close/SKILL.md`; each is one command or one
line, and each is something a Haiku-tier model can do.

1. `progress.md`: prepend one dated entry: `main` hash, merged beads (from `git log
   --merges --since=<last entry>`), what is proven on hardware vs only built, the next bead.
2. `CLAUDE.md` §Current State: replace the body with the three lines "main at `<hash>`
   (`<date>`); status: `.planning/progress.md`; next: `<bead>`". Nothing else.
3. `design/INDEX.md`: for every bead merged this session, set the row's Status to
   "Live, built `<merge hash>`"; for every new design, add the row.
4. `decisions/INDEX.md`: any ADR whose implementation landed → drop "not yet implemented".
5. `roadmap.md`: if a milestone changed state, change its marker; if a "Live risk" was
   retired, strike it with the retiring commit.
6. `.research/findings/INDEX.md`: index any capture or measurement written this session.
7. Dangling references: run the doc-link loop from Appendix A locally.
8. Build-file comments: `grep -n 'never launched\|not yet\|NOT called anywhere\|default OFF' firmware/CMakeLists.txt firmware/sdk-patches/README.md`
   — read each hit; delete or date it.
9. Agent files: if a rule changed (merge policy, tools, paths), change it in `CLAUDE.md`
   *and* in `.claude/agents/*.md` *and* in `AGENTS.md` in the same commit.
10. Machine notes: anything with `/Users/`, `/Applications/`, "this Mac" goes in the
    machine-notes file, never in `CLAUDE.md`.
