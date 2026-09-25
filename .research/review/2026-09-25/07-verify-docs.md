# 07 — Adversarial verification of `06-docs-build-process.md`

Verifier seam: `verify-docs`. Tree: `main` = `2e37164` (HEAD `1104a58` adds only review files;
`git diff --stat 2e37164 HEAD` touches `.research/review/` only). Full history present (not shallow,
503 commits on `main`). Verified 2026-09-25.

Legend: ✅ verified · ❌ wrong · ⚠️ imprecise (how is stated). "Line off" means the claim is right but
the cited line number is wrong. That matters because a cheaper model will edit at that line.

## 0. Summary

| Finding | Verdict | Rows checked | ❌ | ⚠️ |
|---|---|---|---|---|
| F-docs-01 (CLAUDE.md Current State + currency table) | **CONFIRMED** (one row wrong, a few imprecise) | 44 | 1 | 7 |
| F-docs-02 (no CI; merges are gated on CI) | **CONFIRMED**. Appendix A needs corrections (§2) | 5 | 0 | 2 |
| F-docs-03 (roadmap stale) | **CONFIRMED** | 11 (shared with 01) | 0 | 2 |
| F-docs-04 (progress.md stale) | **CONFIRMED** (open-bead count imprecise) | 9 (shared with 01) | 1 | 2 |
| F-docs-06 (design/INDEX status column) | **PARTIALLY**: 5 of 7 rows right; 2 overstated; the proposed fix text for one row is wrong | 7 | 0 | 2 |

Across the five P1s, **1 row is wrong** (the watchdog "never trips" row). About 10 rows are imprecise:
wrong line numbers, wrong counts, or a partly resolved item reported as fully resolved. One piece of
proposed replacement text is wrong (F-docs-06, composite-damage row). One omission: two more `eabihf`
sites were missed (F-docs-08).

---

## 1. Per-P1 verification

### F-docs-01 — CLAUDE.md "Current State" stale — **CONFIRMED**

`CLAUDE.md` §Current State is at lines 597-647 ✅. `git rev-list --count 6088202..main` = **419**, so the
report's "400+" ✅. CLAUDE.md last commit is `45a56d4` 2026-09-01 ✅.

| # | Doc / claim | Report's reality | Check | Verdict |
|---|---|---|---|---|
| 1 | "`main` is at `6088202`" (CLAUDE.md:602) | `2e37164` 2026-09-24 | `git log -1 main` | ✅ |
| 2 | "`pico-link-a67` DONE, unmerged, unreviewed" | merged `5416c76` 2026-08-28 | `5416c76` is a merge (parents `5bfa09e 1c612b6`), ancestor of main; body: "Reviewed and approved by code-reviewer 2026-08-28" | ✅ |
| 3 | "Audio: diagnosed, not fixed … UNPROVEN" | `91c2432` 2026-08-29; LDAC listenable (progress.md:3-11) | `91c2432` merge, body "PROVEN ON HARDWARE: last_alt=1, streaming=1"; progress.md:3 "LDAC is listenable" | ✅ |
| 4 | "Display: half-fixed … Not merged" | `36a6c4f` merged; MADCTL 0x60 at st7789.c:142 | `36a6c4f` merge (pico-link-zzq); `st7789.c:186 madctl_param = 0x60`, comment :142; cable-right open :171-178 | ✅ |
| 5 | "`pico-link-6o2` bt.c:102 calls Rust from IRQ" | fixed; main.c:644; bt.c ring | `0afe5cf`/`f13652a`/`504c967`; `bt.c:52-89` MPSC ring; `main.c:644` drain comment | ✅ |
| 6 | "`pico-link-gap` panic recorder" (open) | `panic_recorder.c`, `PICO_PANIC_FUNCTION` CMake:444 | file exists; `8a06fa7`; `PICO_PANIC_FUNCTION=pl_panic_c_hook` at **CMakeLists.txt:443** | ✅ (line off by 1) |
| 7 | "`pico-link-yz6`" listed open | rejected by ADR 2026-09-02 §Decision 1 | `decisions/2026-09-02-core1-allocation…md:97` "Do not move the display and UI to core1"; INDEX:15 | ✅ |
| 8 | "`pico-link-d7k`" (no automated input path) | `debug_remote.c` + `PL_DEBUG_REMOTE` (bead cd3) | The automation gap is closed (`f83b791`, `aa2f25e`, CMake:121). But **d7k's actual subject** (progress.md:364, roadmap:205-208) is proving d-pad-select → `PL_CMD_CONNECT` **on hardware**, and nothing in git records that proof. | ⚠️ gap closed; d7k itself unproven |
| 9 | "**Next:** code-reviewer on a67 … then icb on hardware" | both closed in August | rows 2-3 | ✅ |
| 10 | CLAUDE.md:51 `eabihf` | CMake uses `eabi` | `CMakeLists.txt:175 set(UI_FFI_TARGET_TRIPLE thumbv8m.main-none-eabi)` (**line 175, not 180**) | ✅ (line off) |
| 11 | CLAUDE.md:56-69 layout lists only core/emulator | ui-ffi, firmware, spike, tools, fixtures absent | `sed -n 56,66p CLAUDE.md`; `ls -d */` | ✅ |
| 12 | CLAUDE.md:330-343 BOOTSEL vs CDC `--bootsel` | two answers | CLAUDE.md:305 vs :335 | ✅ |
| 13 | CLAUDE.md:293-301 `.research/findings/` convention unused | INDEX "(none yet)" | CLAUDE.md:300; findings/INDEX.md last commit `c2de3f3` 08-26 | ✅ |
| 14 | roadmap "Last updated 2026-08-28" | 260 commits since | roadmap:3; last commit `eab223b` 08-28 | ✅ |
| 15 | roadmap "A — Repo reset (in progress)" | done | roadmap:89 vs progress.md:74 "Epic A — repo reset ✅" | ✅ |
| 16 | roadmap "M3 IN FLIGHT" | done `91c2432` | roadmap:137; M3 commit `92fe3f9` is an ancestor of main | ✅ |
| 17 | roadmap D1 "Rust owns SPI … now that Rust owns the binary" | contradicts C-first | roadmap:143-144 vs :215; SPI is in `firmware/src/st7789.c` (C) | ✅ |
| 18 | roadmap D2 "header we author" | done, cbindgen, EVENT=5, COMMAND=2 | I generated the header: `PL_EVENT_ABI_VERSION 5`, `PL_COMMAND_ABI_VERSION 2` (also `PL_RENDER_ABI_VERSION 1`) | ✅ |
| 19 | roadmap D3 screens | all present | `render/hero.rs`, `render/wizard.rs`, `app/screens/devices.rs`, `app/screens/settings.rs` exist | ✅ |
| 20 | roadmap live risk "SPI 1MHz, ~1.008s blit" | 75 MHz / 38.6 ms | roadmap:182-183; progress.md:298-299 (`8381c2b`) | ✅ |
| 21 | roadmap live risk "No automated input path (d7k)" | `PL_DEBUG_REMOTE` | same caveat as row 8 | ⚠️ |
| 22 | roadmap:192 `eabihf` | `eabi` | roadmap:192 | ✅ |
| 23 | roadmap provenance header + gate | "4 of 24 … lack header; no gate" | **25** `.c` files in `firmware/src`, not 24; the 4 without a header are `debug_remote.c`, `input.c`, `media_keys.c`, `volume.c` ✅; no gate in tools/hooks/CMake ✅ | ⚠️ count |
| 24 | progress.md "`main` @ `1e4734c`" | `2e37164` | progress.md:5; `git rev-list --count 1e4734c..main` = 261 | ✅ |
| 25 | progress "Waiting on Andreas … nxf unproven by ear" | 3.5 weeks old | progress.md:17-18; `1e4734c` merged nxf 08-31. The claim is stale by date; whether he listened is unrecorded. | ✅ |
| 26 | progress second "Last updated: 2026-08-28" | two stamps | progress.md:46 | ✅ |
| 27 | progress "One step left: mv esp32-bluetooth-tx" | done | progress.md:83 | ✅ |
| 28 | progress `PICO_SDK_PATH=/Users/andreas/pico-sdk` | others say `~/.pico-sdk/sdk/2.1.1` | progress.md:387 vs **CLAUDE.md:536** (not 541), `hardware-debugger.md:92`, `apply-sdk-patches.sh:9` (not :2) | ✅ (lines off) |
| 29 | progress "Open beads: zzq, cz0.4, d7k, gap, hfc … all but hfc resolved" | | cz0.4 ✅ resolved (`91c2432`), gap ✅ resolved. **zzq is only half resolved**: the mirror is fixed, but "cable exits right" is still open (`st7789.c:171-178`, "tracked as follow-up work"). d7k is unproven (row 8). The list also names `46w`/`1rp`. | ⚠️ |
| 30 | progress:416 "watchdog armed 8s, fed every 3s" → "observe-only … **never trips**" | | The progress claim is indeed stale: the timeout is now **2000 ms** (`watchdog_sup.c:61`) and the feed is once per superloop (`main.c:950`). But **"never trips" is wrong.** The hardware watchdog is armed (`watchdog_sup.c:297 watchdog_enable`) and fed only from the superloop (`watchdog_sup.c:355,365`), so a wedged superloop still resets the board in 2 s. The `PL_WDT_ENCODER` subsystem also trips even in observe-only (`watchdog_sup.c:344-346 pl_panic_record_watchdog_stale`). | ❌ |
| 31 | README "Status: early …" | firmware streams LDAC … | README.md:14; last commit `90fa816` 08-27 | ✅ |
| 32 | README §Building cargo only | no firmware steps | README.md:35-40 | ✅ |
| 33 | AGENTS.md `--status in_progress` vs `--claim`; push MANDATORY vs "do not push" | contradictory | AGENTS.md:10 vs :52/:108; :17-36 vs :68 | ✅ |
| 34 | decisions/INDEX RenderCtx "not yet implemented" | `ctx.rs` added `a5e8f33` 08-31 | INDEX:14; `git log --diff-filter=A -- core/src/render/ctx.rs` = `a5e8f33` 2026-08-31; `widget.rs:303,343,371,404,439,523,576` take `&RenderCtx` | ✅ |
| 35 | on-device-ui §21 E8 "not yet implemented" | implemented | **line 697**, not 692 | ✅ (line off) |
| 36 | `.research/findings/INDEX.md` "(none yet)" | captures unindexed | `.research/captures/2026-08-31-ldac-dwell-fix/` exists | ✅ |
| 37 | rust-embedded-supervisor.md:155,169 `eabihf` | `eabi` | confirmed | ✅ |
| 38 | rust-embedded-supervisor.md:181-185 "no firmware crate yet" | ui-ffi/firmware exist | at **:186** | ✅ |
| 39 | tester.md:66 "once Epic C lands, the USBPods firmware fork" | never forked | at **tester.md:82** | ✅ (line off) |
| 40 | sdk-patches/README §04 "(EXPERIMENT, default OFF)" | CMake ON, tusb_config ON | README:107; `CMakeLists.txt:128 option(PL_USB_ISO_XFER_ISR … ON)` (**128**, not 131); `tusb_config.h:196 #define PL_USB_ISO_XFER_ISR 1` | ✅ |
| 41 | CMake comments `:348`, `:419`, `:426`, `:540` say core1 never launches | `a2dp.c:2556` launches it | `:419`, `:426-427`, `:540` ✅ stale. **`:348` is correct**: it describes the `-DPL_ENCODER_ON_CORE1=OFF` fallback build ("core1 is never launched (pl_a2dp_launch_core1 does not even exist in that build)"). Do not "fix" it. | ⚠️ 3 of 4 |
| 42 | CMake:430-434 `PICO_CORE1_STACK_SIZE` rationale inert | explicit stack | `a2dp.c:2512 s_core1_stack[1024]`, `:2556 multicore_launch_core1_with_stack` | ✅ |
| 43 | core/Cargo.toml:15 cites missing ADR | missing | `ls .planning/decisions/` has no `portability-boundary…`; the doc-link check (below) reports it dangling | ✅ |
| 44 | st7789.c:1-2 cites `firmware-spike/src/main.rs:285-410` | true today | confirmed | ✅ |

**Omission (belongs to F-docs-08):** `eabihf` also appears in root **`Cargo.toml:6` and `:15`** and in
**`ui-ffi/src/lib.rs:194`**. §2 of the report even praises `Cargo.toml:4-8` as "a clear comment", but that
comment names the wrong triple. F-docs-08's verification grep (`CLAUDE.md .planning .claude`) would pass
while these three sites stay wrong. Use `grep -rn eabihf --exclude-dir=target --exclude-dir=firmware-spike --exclude-dir=.research .`
and keep only the hits outside `progress.md:274` and `CMakeLists.txt:173`, which mention eabihf on purpose.

**Proposed replacement (5-line pointer, or delete):** sound. Everything in the section is either in
progress.md or stale. The replacement must not copy progress.md's own stale "Open beads".

### F-docs-02 — No CI; CLAUDE.md and auto-run gate merges on it — **CONFIRMED**

| Claim | Check | Verdict |
|---|---|---|
| `.github/` absent | `ls .github` → No such file | ✅ |
| CLAUDE.md:93 "merges … once CI passes" | the text is at **CLAUDE.md:97** ("CI green + reviewed") and **:108**; line 93 is about code-reviewer | ⚠️ line off |
| auto-run step 4 "merge … when CI is green" | `.claude/skills/auto-run/SKILL.md:22` (and :48) | ✅ (line off) |
| "139 merge commits on main" | `git log --merges --oneline main \| wc -l` = **146** | ⚠️ count |
| "Nothing in the tree proves `ui-ffi` still cross-compiles at `2e37164`" | Now proven here. A fresh `CARGO_TARGET_DIR` build of `cargo build -p ui-ffi --release --target thumbv8m.main-none-eabi` exits 0, and the **full firmware links** (§2). | The finding still stands, because nothing re-proves this on each merge. |

### F-docs-03 — roadmap.md describes the August project — **CONFIRMED**

Rows 14-23 above. Also `vision-session/SKILL.md` step 1 is at **:22** (not :20) ✅ in content.
Proposed status table: "A ✅, B ✅, C ✅ M1–M5, D1–D3 ✅, D4 ⬜ not recorded, E ⬜" is consistent with git.
M5 persistence is `firmware/src/persist.c`, and D4 has no record (see §3). One addition: D1's correction
should say "C owns SPI/DMA/panel (`st7789.c`); Rust renders into a framebuffer and returns a damage rect
(`pl_ui_render_ex`)". Do not write only "fix D1's text".

### F-docs-04 — progress.md stopped 2026-08-31 — **CONFIRMED**

`git log -1 -- .planning/progress.md` = `e8569ef` 2026-08-31 ✅. `git rev-list --count e8569ef..main` = 260 ✅.
The monthly split is 243 before 09-01 and 260 after ✅. Report numbers are ±1.
"Six of the eight Open beads (lines 352-378) are resolved": ⚠️. The section names 7 beads (zzq, cz0.4,
d7k, gap, hfc, plus 46w/1rp, which it says are "NOT ON THE BOARD"). Clearly resolved: cz0.4 and gap.
Partly resolved: zzq (cable-right still open). Unproven: d7k. Open: hfc. Unknown: 46w and 1rp.
The watchdog paragraph (row 30) is stale but must not be replaced with "never trips".
`session-close` writes a memory file, not progress.md (`SKILL.md:54-73`) ✅.

### F-docs-06 — design/INDEX status column — **PARTIALLY CONFIRMED**

The status column sits at INDEX:21-24, 33, 35 and 50. The report's line numbers (21-24, 36, 39, 51) are off for the last three.

| Row | Report claim | Check | Verdict |
|---|---|---|---|
| volume-on-display "unbuilt (4v2.6)" | built `2c646a2` 09-07 | merge, subject "(bead pico-link-4v2.6, VT6)" | ✅ |
| ldac-quality-selector "unbuilt (7jol.2)" | built `afa341b` | merge "the QUALITY row, its picker, and Home's ADAPTIVE tag (bead pico-link-7jol.5)" | ✅ |
| audio-fault-model "unbuilt (9eq2.3)" | `15185f1`, `5cb6048` | both merges (9eq2.3.1, 9eq2.3.2); `firmware/src/fault.c` exists | ✅ |
| home-fault-strip v2 "unbuilt (9eq2.2)" | `5efba31` | merge (9eq2.3.3); `render/fault_glyph.rs`, `app/screens/why_page.rs` exist | ✅ |
| link-state-vs-discovery "unbuilt (88xs)" | `3818d75` | merge (88xs.1); `BtModel::discovering` (model.rs:46), `LinkGlyph` in widget/screen/home.rs | ✅ |
| core1-encoder-default "does NOT flip default" | flipped `0c643f1` | The **Status** column (INDEX:50) already reads "Live, implemented on pico-link-nli.9". "Does NOT flip" is in the *What it settles* column and correctly describes that doc's own scope. The flip came later, from a different bead (nli.10). The row is missing a pointer, not wrong. | ⚠️ |
| composite-damage-and-paint-plan "Live" → "Design NOT built" | `PaintPlan` absent | `grep -rn PaintPlan core/src` = 0 ✅. **But stage 1 (B1) landed in the reviewed tree:** `ae557c1` (2026-09-24, inside `2e37164`), "Folds in yn5i.1's B1: HomeView forwards damage_hint/damage_region_key". HomeView now overrides both (`home.rs:695,719`). HomeView is also no longer rebuilt (`Refresh::Keep`), which removes Home's exposure to the B2 rebuild path (not traced end to end). | ⚠️ partly built |

The title says "seven merged designs 'unbuilt' and one unbuilt design 'Live'", but only **5** rows say
"unbuilt". The seven are 5 + core1 + composite, and the last two are overstated.
**Proposed fix text is wrong for composite-damage.** "Design of record, unbuilt (no bead)" is incorrect:
the bead is `pico-link-4ube` (doc line 3) and stage 1 is built. Use instead: "Design of record; stage 1 (B1)
built `ae557c1` (yn5i.1); `PaintPlan` + per-segment meter regions unbuilt (`pico-link-4ube`)".

---

## 2. Appendix A (proposed CI) — run here

Environment: Ubuntu 24.04 container, cargo 1.94.1 / rustfmt 1.8.0 / cbindgen 0.29.4 / cc 13.3.0.
For the firmware job I installed `gcc-arm-none-eabi libnewlib-arm-none-eabi libstdc++-arm-none-eabi-newlib`
from apt (13.2.rel1) and cloned pico-sdk 2.1.1 into scratch. All outputs went to the scratch directory.
The generated `firmware/include/` was removed afterwards, and `git status` is clean.

| Step | Result today | What to change |
|---|---|---|
| `cargo test --workspace` | **PASS**: 578 passed / 0 failed (12 result lines, summed) | none |
| `cargo clippy --workspace --all-targets -- -D warnings` | **FAIL** (exit 101). With `-D`, clippy stops at `pico-link-core`. Without `-D`: **4 distinct lints** (5 emissions). The charter/report "7 warnings" counts 3 cargo summary lines. The lints: `derivable_impls` core/src/power.rs:118; `type_complexity` ×2 core/src/run.rs:1245,1299 (tests); `dead_code` `pl_ui_panic_hook` ui-ffi/src/lib.rs:262 (host-only; needs a `cfg`, not deletion) | Fix the 4 lints first, or put `continue-on-error: true` on this step until then. **As written, the job aborts here, so fmt, cross-compile, cbindgen, the C tests and doc-links never run on day one.** Put the passing steps before clippy/fmt. |
| `cargo fmt --all --check` | **FAIL**: 17,295-line diff, 70 files, 1,131 hunks (report ✅). No `rustfmt.toml`. | One `cargo fmt --all` commit first, or `continue-on-error` |
| `cargo build -p ui-ffi --release --target thumbv8m.main-none-eabi` | **PASS** (fresh target dir) | none |
| (task item) `cargo build -p pico-link-core --target thumbv8m.main-none-eabihf` | **PASS** for both `eabi` and `eabihf`, for both `pico-link-core` and `ui-ffi`. Rust `no_std` builds for either triple; `eabihf` fails only at the C link (CMakeLists.txt:170-175). So a supervisor that "verifies" with eabihf gets a false green. | CI must use `eabi` (the appendix does) |
| cbindgen header (`cargo install cbindgen --locked`, run in `ui-ffi/`) | **PASS**: 1,263-line header, 3 benign WARNs | Consider pinning `--version 0.29.4`. The header is not committed, so a version drift changes the ABI text silently. |
| Host C tests, commands exactly as the appendix gives (8/8 tried) | **PASS 8/8** *given the order in the appendix* (header generated first). Without the header, t7 fails with `pico_link_ui.h: No such file` (report's 7/8 ✅). **All 8 emit `-Wcomment` "multi-line comment"**, because each file's documented `cc` line ends in `\` inside a `//` comment. | Harmless now. Adding `-Werror` would break all 8: use `-Wno-comment` or change the comment style. |
| Doc links resolve | **FAIL** on day one: `dangling: .planning/decisions/2026-08-11-portability-boundary-and-workspace-split.md` (core/Cargo.toml:15). The appendix does not label this step as failing. It checks only repo-root-relative `.planning/(decisions\|design)/…` paths (29 unique), not relative INDEX links. | Fix core/Cargo.toml:15 first or mark the step expected-to-fail |
| Firmware: `PICO_SDK_FETCH_FROM_GIT*` env | **Dead weight**. `pico_sdk_import.cmake:28-31` prefers `PICO_SDK_PATH`, which the step exports. On its own (tested: `env -u PICO_SDK_PATH PICO_SDK_FETCH_FROM_GIT=1 …_TAG=2.1.1 cmake -S firmware …`), configure **FAILS**: `CMakeLists.txt:31 pico-sdk is MISSING a vendored patch (marker pl_ep_double_arm_count)`. A fetched SDK is unpatched. | Drop the three env vars. The explicit clone + patch is the only one-pass path (appendix note 1 is right). |
| `git clone --depth 1 --branch 2.1.1` + submodules | **PASS** (~22 s; tinyusb `86ad6e5`, btstack `501e6d2`, cyw43 `c1075d4`) | none |
| `tools/apply-sdk-patches.sh` on pristine 2.1.1 | **PASS**: applies 01, 02, 03, 04a-d, 05, 06 (6 files, +164/-4). A second run prints "already applied" for all (idempotent). Mode 100755. | none. This retires the report's §7 item 2 "unverifiable". |
| ARM toolchain | `carlosperate/arm-none-eabi-gcc-action` **untested here**. **apt works**: `gcc-arm-none-eabi libnewlib-arm-none-eabi` on ubuntu 24.04 gives 13.2.rel1 with newlib, and it built the firmware. | Prefer apt (no third-party action) |
| `cmake -S firmware -B build -G Ninja && cmake --build build` | **PASS**: 246 steps, ~45 s, `pico_link.uf2` = **1,395,200 bytes**; `check_ldac_not_in_flash.cmake: OK -- all 98 checked libldac symbols resolved outside flash`. Configure warns "No installed picotool … building from source" (FetchContent, needs network; adds build time). 44 compiler warnings, all `"ENABLE_CLASSIC" redefined`. | Optionally cache or pre-install picotool 2.1.1. `continue-on-error: true` can start as `false`: it passed first try on Linux. This retires the report's §7 item 1. |

## 3. MVP ruling check

The MVP (CLAUDE.md:14) is "pair fresh headphones and see the live codec and bitrate on screen, driven
entirely by the buttons". The report's ruling is **"PARTLY / likely yes, unproven in the record"**.
**I agree.** Every leg is in code; hardware acceptance (roadmap D4) is not recorded anywhere.

| Leg | Code path |
|---|---|
| Buttons → NavIntent | `firmware/src/main.c:574-576` `pl_link_input_poll` → `pl_ui_input` (`ui-ffi/src/lib.rs:530`) → `NavIntent` (`core/src/input.rs:21`) |
| Home → Devices → "Pair new headphones" | `render/home.rs:376` pushes `build_devices_screen`; `app/screens/devices.rs:107` row; `:172-180` → `Command::StartScan` + wizard `Scanning` |
| Scan on radio | `bt.c:1008` `PL_COMMAND_TAG_START_SCAN` → `bt.c:543 gap_inquiry_start` |
| Pick device → connect | `render/wizard.rs:308-316` `on_activate_index(Verb::Pair)` → `Command::Connect` + `WizardPhase::Connecting`; `bt.c:1022` → `a2dp.c:2963 a2dp_source_establish_stream` |
| Reaches connected | `a2dp.c:3646 pl_bt_push_connect_succeeded` → `bt.c:361-368` event → `app/fold.rs:141-146 on_connect_succeeded` → `WizardPhase::Succeeded`, `Command::PersistDevice` |
| Codec word | `bt.c:389 PL_EVENT_TAG_CODEC_CHANGED` → `fold.rs:302 model.connected_codec = Some(codec)` → `home.rs:266-292` |
| Live kbps | `a2dp.c:4145 PL_EVENT_TAG_LDAC_BITRATE_CHANGED` → `fold.rs:322-324 ldac_live_kbps` → `home.rs:291-292 BitrateStatus::Kbps` → `hero.rs:912 "{kbps} kbps"` |
| Test driving it by NavIntent | `app/tests.rs:472-484`: Select ×3 (Home → menu → Bluetooth → Pair) → `ConnectSucceeded` → `Succeeded` |
| No console needed | `PL_DEBUG_REMOTE` default OFF (`CMakeLists.txt:121`) |

Caveats the report did not mention:
1. For non-LDAC codecs the "live" bitrate is the **nominal** figure (`home.rs:291`).
2. `BitrateStatus::Idle` ("idle", never "0 kbps") has **no production constructor**. It appears only in `hero.rs:519,911` formatting and in tests, so the design's "host silent" state is not wired. This does not block the MVP but is worth a P2.
3. The strongest counter-evidence in the record is the `75495f1` merge body (2026-09-02): "Hardware confirmation -- pair, power-cycle, still listed by name -- is still outstanding". No later commit or doc records it.

**Ruling:** code-complete, not proven on hardware. The report's Q2 to Andreas ("Was D4 ever done?") is the right closing question.

## 4. P2/P3 findings with a refutable factual basis

- **F-docs-09 (watchdog): factual basis partly REFUTED.** "Never trips" and "every hang … that needed a physical BOOTSEL press is a hang the watchdog was designed to recover from and was configured not to" are overstated. The HW watchdog is armed (2000 ms, `watchdog_sup.c:61,297`) and fed only at `main.c:950` via `pl_wdt_service`. A superloop hang therefore still resets the board. Observe-only suppresses only *per-subsystem* staleness trips, and `PL_WDT_ENCODER` trips even then (`watchdog_sup.c:344-346`). The step-7 point (no `PL_DIAG_WDT_TEST`, the default still ON) stands.
- **F-docs-10: the "Linux-generic form" is REFUTED.** `PICO_SDK_FETCH_FROM_GIT=1 PICO_SDK_FETCH_FROM_GIT_TAG=2.1.1` does **not** work with `pico_sdk_import.cmake`. Configure dies at `CMakeLists.txt:31` because the fetched SDK is unpatched (measured). The README steps must clone, patch, then configure. The cbindgen-undocumented claim ✅ (`grep cbindgen CLAUDE.md README.md .claude/agents/*.md` is empty).
- **F-docs-11: ⚠️ framing.** `apply-sdk-patches.sh:6-7` itself says "The .patch files … remain the human-readable source of truth", and `sdk-patches/README.md` has sections for 04, 05 and 06. So a reader of the README does not "miss three patches"; only the `.patch` files are missing. The README §04 "default OFF" point ✅.
- **F-docs-13: ⚠️** 73 first-parent non-merge commits ✅. Code commits among them: 10, including the initial commit `c2de3f3`, so 9 after it. **6 of those 9 carry a bead id** (`67f1d94`, `c005a84`, `eb1c4db`, `edd2a99`, `306904e`, `8b10514`). A fast-forward merge of a `bd-*` branch is indistinguishable from a direct commit in `--first-parent`. Only `fe8ce44`, `e29b899` and `68a4a34` evidence work with no bead.
- **F-docs-16: ⚠️** "139 merges" → 146.
- **F-docs-17: ⚠️** firmware-spike is 12 tracked files, **1,266** lines of .rs/.c/.h (not 1,087). Last touched `bbe18a7` 2026-08-27 ✅.
- **F-docs-20: ⚠️** "7 warnings" → 4 distinct clippy lints (see §2). "Unverifiable here" §7 items 1-2 are now verified (ui-ffi cross-compiles, patches apply cleanly, the firmware links).
- **F-docs-21: ⚠️** the picotool comment is at `CMakeLists.txt:368`, not 396-398. `.gitignore` settings line is :36, not :29 (F-docs-15). Other nits ✅: `hardware-debugger.md` `model: claude-opus-5`; 10 + 19 screenshots; `core/Cargo.toml:15`.
- Not refuted (spot-checked ✅): F-docs-05, -07, -12, -14 (`/Users/` in CLAUDE.md), -15 (settings.json gitignored, `.codex/hooks.json` exists), -18 (`pico_sdk_import.cmake:48-50` defaults to `master`), -19 (only `firmware/vendor/libldac/{LICENSE,NOTICE}`).

## 5. Guidance for the model that edits the docs

- Use the line numbers in this file, not 06's, for CLAUDE.md:97/108/536, CMakeLists.txt:121/128/175/443, tester.md:82, on-device-ui.md:697.
- Do **not** edit `CMakeLists.txt:348`. It is correct.
- Do **not** write "watchdog never trips" anywhere.
- Add root `Cargo.toml:6,15` and `ui-ffi/src/lib.rs:194` to the eabihf sweep.
- For the CI: fix the 4 clippy lints and run one fmt commit, or order the passing steps first. Fix `core/Cargo.toml:15` before enabling the doc-link step. Use apt for the ARM toolchain, drop the `FETCH_FROM_GIT` env vars, and the firmware job can be required from day one.
