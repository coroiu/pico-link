# Pico Link architecture & implementation review — charter (2026-09-25)

Reviewed tree: branch `claude/gallant-goldberg-pxdjvq` at `2e37164` (== main tip as of 2026-09-24).

## Purpose
Produce findings that a *cheaper* model (Haiku/Sonnet) can act on without re-deriving
context. Every finding must therefore be anchored (file:line), evidenced (what the code
does, quoted), and verifiable (a command or test that shows the fix landed).
"Everything is fine here" is a valid and valuable result — say so and say why.

## Baseline in this container (measured 2026-09-25)
- `cargo test --workspace`: 578 passed, 0 failed (core 478, ui-ffi 58, emulator 27+2+3+3, core integration 1+1+5).
- `cargo clippy --workspace --all-targets`: 7 warnings.
- No `.github/workflows` — there is no CI.
- No ARM toolchain, no `PICO_SDK_PATH`, no `bd`: the firmware cannot be cross-compiled here.
  Host-side C tests in `firmware/tests/*.c` are standalone; each file's header comment says how to build it with gcc.
- Hardware is not available. Any claim that needs a board is marked **unverifiable-here**, not asserted.

## Scope
- IN: `core/`, `ui-ffi/`, `emulator/`, `firmware/src`, `firmware/tests`, `firmware/sdk-patches`, `firmware/CMakeLists.txt`, `firmware/cmake`, `tools/`, `.planning/`, `.research/`, `CLAUDE.md`, `AGENTS.md`, `README.md`, `.claude/`.
- OUT: `firmware/vendor/libldac` (Sony, reviewed only at its call boundary), `build/`, `target/`.
- `firmware-spike/`: one question only — delete or keep, with evidence.

## Severity
- **P0** — wrong behaviour, memory-safety, data loss, or a hang path on the shipping path.
- **P1** — real defect or architectural debt that will bite within the roadmap (MVP + next milestone), or an ADR the code silently violates.
- **P2** — maintainability / clarity / test-gap that costs future work but is not currently wrong.
- **P3** — nit. Batch these; do not write one finding per nit.

## Confidence
- **High** — read the code path end to end and (where possible) ran it.
- **Medium** — read the code, reasoning is sound, one link not directly verified.
- **Low** — plausible, needs hardware or a deeper trace. Still report it, marked Low.

## Finding format (mandatory — synthesis is mechanical)
```
### F-<seam>-<nn>: <one-line title>
- Severity: P0|P1|P2|P3   Confidence: High|Medium|Low   Effort: S|M|L   Tier: Haiku|Sonnet|Opus
- Location: path:line[, path:line]
- Evidence: what the code does today; quote ≤6 lines
- Why it matters: consequence, concretely
- Fix sketch: the shape of the change, not the patch
- Verification: command/test that proves it fixed (existing test to extend, or new test to write)
- Related: ADR / design doc / bead IDs mentioned in comments, if any
```
`Tier` = the cheapest model that can safely make this change unsupervised.
Haiku: mechanical, single-site, covered by an existing test. Sonnet: local reasoning, may add a test.
Opus: cross-file design change or anything touching IRQ/core1/FFI ownership.

## Report layout (each seam writes `NN-<seam>.md`)
1. **Verdict** (≤5 lines): overall health of this seam, one sentence on what is genuinely good.
2. **What is well done** — be specific; the synthesis needs to know what NOT to touch.
3. **Architecture assessment** — does the structure match the ADRs / design docs it cites? Where does it diverge, and is the divergence a bug in the code or in the doc?
4. **Findings** — ordered by severity. Follow the format above exactly.
5. **Test coverage** — what is covered, what is not, what a cheap model could add.
6. **Open questions for Andreas** — only things that need a product or hardware call.
7. **Unverifiable here** — claims that need the board.

## Rules for reviewers
- Read the code. Do not grep-and-guess. Trace the path before naming a defect.
- Run what is runnable (`cargo test`, `cargo clippy`, gcc host tests). Cite counts.
- Do not edit source, do not commit, do not create branches. Write only your report file.
- Quote small. Cite `path:line`. The reader has the repo.
- No finding without a Verification line. If none is possible, say "manual on hardware" and mark Low.
- Prefer fewer, deeper findings over a long list of shallow ones.
- If a design doc and the code disagree, say which one you think is right and why.
