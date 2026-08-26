# pico-link

## Project Overview

**Pico Link is a USB-to-Bluetooth audio dongle with a screen.** Plug it into any
computer; the host sees a driverless USB sound card, and the dongle streams that
audio to Bluetooth headphones over A2DP with a hi-res codec — LDAC first.

What makes it ours is the **display**. Comparable dongles are blind: a button, an
LED, a serial console if you're lucky. Pico Link has a 240x240 screen and a d-pad,
so pairing, device switching, codec selection and live link status happen on the
device with no terminal attached.

**MVP:** pair fresh headphones and see the live codec and bitrate on screen,
driven entirely by the buttons.

Full plan, milestones and live risks: `.planning/roadmap.md`.

## Hardware

- **Pimoroni Pico Plus 2 W** — RP2350B, dual Cortex-M33 @150MHz, 520KB SRAM,
  **8MB PSRAM**, 16MB flash, Raspberry Pi RM2 radio (Bluetooth 5.2 Classic + LE),
  USB-C.
- **Waveshare Pico-LCD-1.3** — 240x240 IPS, ST7789, 4-wire SPI.
  Display: GP8 DC, GP9 CS, GP10 SCK, GP11 MOSI, GP12 RST, GP13 BL.
  Joystick: GP2 up, GP18 down, GP16 left, GP20 right, GP3 press.
  Buttons A/B/X/Y: GP15, GP17, GP19, GP21.
- A stock **Raspberry Pi Pico 2 W** is the known-good reference board.

**Why RP2350 and not ESP32:** A2DP (and therefore LDAC) needs Bluetooth Classic
BR/EDR. The ESP32-S3 is BLE-only and Espressif closed the BR/EDR request as
"Won't Do"; the original ESP32 has BR/EDR but no USB peripheral, so it can't be a
sound card. RP2350 + CYW43439 has both.

## Tech Stack

- **Languages**: Rust (stable, 2021 edition) above the codec layer; C below it.
- **Firmware**: fork of USBPods (github.com/wasdwasd0105/USBPods-Pico2W) —
  GPL-3, pico-sdk 2.1.1, BTstack, TinyUSB, Sony libldac (Apache-2.0), FDK-AAC.
  **BTstack and libldac stay C behind FFI** — there is no Rust Bluetooth Classic
  host stack, and writing one is not a project.
- **Shared core**: `embedded-graphics`, heading for `no_std` + `alloc`.
- **Desktop emulator**: minifb (windowed), headless, PNG capture.
- **Licence**: GPL-3, inherited by forking USBPods. Their
  `LICENSE-EXCEPTIONS.md` grants a section-7 linking exception that already names
  the Rust `core`/`compiler-builtins` libraries.

## Repo layout

- `core/` — platform-free application core. The compiler-enforced portability
  boundary: it must never depend on a platform crate.
  - `render/` — framebuffer, widget/focus model, screen + navigator, vertical
    list, menu, theme, chrome, message, confirm.
  - `input.rs` — the semantic `NavIntent` vocabulary.
  - `platform.rs` — `DisplaySurface`/`InputSource`/`Clock`/`Storage` trait seams.
  - `run.rs` — the shared run loop: frame budget, idle/sleep tiers, flush errors.
- `emulator/` — the three run modes. UI work never blocks on hardware.

This core came from a previous project (a Bitwarden hardware key) and survived the
pivot because it was genuinely decoupled — the render layer had zero coupling to
the product layer above it. Keep it that way.

## The Team (Roles)

Work is done by a small team of specialized agents plus one main-thread persona. Delegate to the right one; don't do their jobs yourself.

| Role | Who | Mechanism | What they own |
|------|-----|-----------|---------------|
| **Vision partner** | Vera | `vision-session` **skill** (main thread) | Long-term product direction & priorities. Talks to Andreas directly. Run via `Skill(vision-session)` — never a subagent. |
| **Task manager / gatekeeper** | Tao | `task-manager` agent (read-only) | Audits the beads board: status hygiene, dependency correctness, orphans, stale work. Reports; does not mutate the board. Hard gates are enforced by hooks. |
| **Architect** | Ada | `architect` agent | System design + **architectural sustainability / anti-quick-fix** guardian (the C/Rust FFI seam, Bluetooth, audio pipeline, storage). |
| **Frontend architect** | Fern | `fe-architect` agent | The GUI framework: layout engine, render pipeline, component model, input/navigation model, presentation-surface abstraction. |
| **UX designer** | Uma | `ux-designer` agent | Interaction & visual design for the 240x240 display + d-pad/buttons; new-feature UX ideas. |
| **Implementer** | Ruby | `rust-embedded-supervisor` agent | Writes the actual Rust (firmware + emulator + shared libs) in a worktree. Restartable. |
| **Tester** | Tess | `tester` agent | The three run modes (headless/windowed/real-target); proves changes work via builds, tests, headless screenshots. |
| Support | scout / detective / scribe / code-reviewer / merge-supervisor | agents | Search / bug investigation / docs / code review / merge conflicts. |

**Model tiers** (`model:` in each agent's frontmatter): Opus for Ada, Fern and Uma —
architecture, the render/layout framework and UX are judgment calls that are
expensive to get subtly wrong. Sonnet for Ruby, Tess, Tao, code-reviewer,
scribe, detective, merge-supervisor. Haiku only for scout (pure file discovery).
**code-reviewer must never be cheaper than Sonnet** — it is the quality gate
before the orchestrator merges, and a reviewer that misses defects is worse than
none because it manufactures false confidence. The real cost lever is dispatching
fewer, better-specified agents, not the model tier.

**Design flow:** Vera (what & why) → Ada + Fern (how, sustainably) → Uma (how it feels) → Ruby (build it) → Tess (prove it) → code-reviewer (quality gate) → **orchestrator merges to `main`** (CI green + reviewed).

**Advisory agents** (Vera, Tao, Ada, Fern, Uma) are read-only / report-only — they produce plans, designs, and audits, not commits. **Supervisors** (Ruby, Tess, merge) implement in worktrees under the beads workflow below.

## Why Beads & Worktrees Matter

Beads provide **traceability** (what changed, why, by whom) and worktrees provide **isolation** (changes don't affect main until merged). This matters because:

- Parallel orchestrators can work without conflicts
- Failed experiments are contained and easily discarded
- Every change has an audit trail back to a bead
- Orchestrator merges to `main` once CI passes and review is clean — completed work should not sit unmerged

## Quick Fix Escape Hatch

For trivial changes (<10 lines) on a **feature branch**, you can bypass the full bead workflow:

1. `git checkout -b quick-fix-description` (must be off main)
2. Investigate the issue normally
3. Attempt the Edit — hook prompts user for approval
4. User approves → edit proceeds → commit immediately
5. User denies → create bead and dispatch supervisor

**On main/master:** Hard blocked. Must use bead + worktree workflow.
**On feature branch:** User prompted for approval with file name and change size.

**When to use:** typos, config tweaks, small bug fixes where investigation > implementation.
**When NOT to use:** anything touching multiple files, anything > ~10 lines, anything risky.

**Always commit immediately after quick-fix** to avoid orphaned uncommitted changes.

## Investigation Before Delegation

**Lead with evidence, not assumptions.** Before delegating any work:

1. **Read the actual code** — Don't just grep for keywords. Open the file, understand the context.
2. **Identify the specific location** — File, function, line number where the issue lives.
3. **Understand why** — What's the root cause? Don't guess. Trace the logic.
4. **Log your findings** — `bd comments add {ID} "INVESTIGATION: ..."` so supervisors have full context.

**Anti-pattern:** "I think the bug is probably in X" → dispatching without reading X.
**Good pattern:** "Read src/foo.ts:142-180. The bug is at line 156 — null check missing."

The supervisor should execute confidently, not re-investigate.

### Hard Constraints

- Never dispatch without reading the actual source file involved
- Never create a bead with a vague description — include file:line references
- No partial investigations — if you can't identify the root cause, say so
- No guessing at fixes — if unsure, investigate more or ask the user

## Workflow

Every task goes through beads. No exceptions (unless user approves a quick fix).

### Standalone (single supervisor)

1. **Investigate deeply** — Read the relevant files (not just grep). Identify the specific line/function.
2. **Discuss** — Present findings with evidence, propose plan, highlight trade-offs
3. **User confirms** approach
4. **Create bead** — `bd create "Task" -d "Details"`
5. **Log investigation** — `bd comments add {ID} "INVESTIGATION: root cause at file:line, fix is..."`
6. **Dispatch** — `Task(subagent_type="{tech}-supervisor", prompt="BEAD_ID: {id}\n\n{brief summary}")`

Dispatch prompts are auto-logged to the bead by a PostToolUse hook.

### Plan Mode (complex features)

Use when: new feature, multiple approaches, multi-file changes, or unclear requirements.

1. EnterPlanMode → explore with Glob/Grep/Read → design in plan file
2. AskUserQuestion for clarification → ExitPlanMode for approval
3. Create bead(s) from approved plan → dispatch supervisors

**Plan → Bead mapping:**
- Single-domain plan → standalone bead
- Cross-domain plan → epic + children with dependencies

## Beads Commands

```bash
bd create "Title" -d "Description"                    # Create task
bd create "Title" -d "..." --type epic                # Create epic
bd create "Title" -d "..." --parent {EPIC_ID}         # Child task
bd create "Title" -d "..." --parent {ID} --deps {ID}  # Child with dependency
bd list                                               # List beads
bd show ID                                            # Details
bd ready                                              # Unblocked tasks
bd update ID --status inreview                        # Mark done
bd close ID                                           # Close
bd dep relate {NEW_ID} {OLD_ID}                       # Link related beads
```

## When to Use Standalone or Epic

| Signals | Workflow |
|---------|----------|
| Single tech domain | **Standalone** |
| Multiple supervisors needed | **Epic** |
| "First X, then Y" in your thinking | **Epic** |
| DB + API + frontend change | **Epic** |

Cross-domain = Epic. No exceptions.

## Epic Workflow

1. `bd create "Feature" -d "..." --type epic` → {EPIC_ID}
2. Create children with `--parent {EPIC_ID}` and `--deps` for ordering
3. `bd ready` to find unblocked children → dispatch ALL ready in parallel
4. Repeat step 3 as children complete
5. `bd close {EPIC_ID}` when all merged

## Bug Fixes & Follow-Up

**Closed beads stay closed.** For follow-up work:

```bash
bd create "Fix: [desc]" -d "Follow-up to {OLD_ID}: [details]"
bd dep relate {NEW_ID} {OLD_ID}  # Traceability link
```

## Knowledge Base

Search before investigating unfamiliar code: `.beads/recall.sh "keyword"`

Log learnings: `bd comments add {ID} "LEARNED: [insight]"` — captured automatically to `.beads/knowledge.jsonl`

## Supervisors

Supervisors implement in worktrees under the beads workflow. Advisory agents (see The Team) do not.

- rust-embedded-supervisor (Ruby) — Rust firmware + desktop emulator + shared libs
- tester (Tess) — test harness / the three run modes
- merge-supervisor — merge conflict resolution

## Planning & Research Conventions

This project keeps durable knowledge in version-controlled markdown, separate from the ephemeral beads board. The **scribe** maintains these; the orchestrator and agents read them for context.

- `.planning/progress.md` — current status + next steps. Update at the end of meaningful work.
- `.planning/roadmap.md` — vision, milestones, live risks and settled decisions. The authority this project executes against.
- `.planning/decisions/` — one ADR per file, `YYYY-MM-DD-short-title.md`, indexed in `INDEX.md`. Format: Context / Decision / Rationale / Alternatives / Consequences.
- `.research/findings/` — one research finding per file, `YYYY-MM-DD-topic.md`, indexed in `INDEX.md`. Keep research separate from the decisions it informs.
- Rule of thumb: **research** goes in `.research/`, **decisions** based on it go in `.planning/decisions/`, **status** in `progress.md`. Update the relevant `INDEX.md` whenever you add a file. Don't delete superseded entries — mark them Deprecated/Superseded.

## Project-Specific Operational Notes

### Firmware build (RP2350)

- Firmware is a fork of USBPods: C, CMake, pico-sdk 2.1.1. Build produces a
  `.uf2`; flash by holding **BOOTSEL** while plugging the board in and copying
  the file onto the `RP2350` drive that appears.
- Target board is the **Pimoroni Pico Plus 2 W** (RP2350B + RM2 radio). USBPods
  ships board headers for `usbpods_universal` and `waveshare_rp2350b_plus_w`;
  ours is modelled on the latter. **Keep a stock Pico 2 W flashed with unmodified
  USBPods as the known-good reference** — when the Plus misbehaves, diff against it.
- Use pull-**ups** on GPIO inputs, not pull-downs: RP2350 erratum E9 affects
  pull-downs on GPIO inputs.

### Host build

Plain `cargo build` / `cargo test` at the repo root. The workspace is `core` +
`emulator`, both host-native.

### Desktop emulator management

- Run: `cargo run --bin desktop`.
- **CRITICAL: never `pkill -f "desktop"`** — it can kill Docker Desktop and other
  processes. Stop it by closing the window, or
  `pgrep -f "target.*debug.*desktop"` then `kill <PID>`.

### Three run modes (testability)

The device must be exercisable by agents in three modes — **headless** (no window;
AI drives it and inspects captured screenshots), **windowed** (minifb, for humans
without hardware), and **real target** (the Pico). Tess owns this.

### Environment & workflow gotchas (learned)

These were expensively earned on the predecessor project. The hardware-specific
ones were dropped in the pivot; what remains is platform-independent and still
applies.

- **Rendering-change verification discipline.** "Tests pass + a 1x PNG + the
  binary launches" is INSUFFICIENT evidence for a render change. Inspect
  framebuffers and PNGs at ZOOM (sub-pixel and text-overflow bugs hide at 1x)
  and, for windowed mode, screencapture the LIVE window. Two separate bugs — a
  sub-row text overflow and a fully blank window — both passed the weak checks
  and were caught only by zoomed and live inspection.
- **Windowed mode IS agent-verifiable.** On this Mac the terminal running Claude
  Code has macOS Screen Recording permission, and BOTH the orchestrator AND
  subagents can run `screencapture -x <file>.png` to grab the live minifb window
  and inspect it. Never assume windowed rendering can't be checked.
- **Beads/worktree hygiene.** The board lives in the embedded Dolt DB at
  `.beads/embeddeddolt/`, which is gitignored — there is nothing to commit before
  a merge, and nothing in git to recover the board from. Back it up with
  `bd dolt push` (needs `ssh-add` first; Dolt cannot prompt for a passphrase).
  `git branch -d` may balk because beads auto-syncs to branch tips — verify
  `git log main..<branch>` is empty, then `git branch -D`. When dispatching a
  supervisor, tell it to create its worktree from local `main` and verify the base
  commit (a supervisor once branched off a stale feature branch).
- **Never put backticks in a `bd -d "..."` description** — the shell
  command-substitutes them.
- **`bd list` (no flags) SILENTLY TRUNCATES its output.** It once hid a real open
  bead during a board sweep. For any audit use `bd list --json` (or
  `bd list --status open`). There is no `.beads/issues.jsonl` to cross-check
  against unless JSONL auto-export is enabled in `.beads/config.yaml` — it is
  OFF by default in 1.2.2, and the file is NOT generated.
- **One bead per supervisor dispatch.** Bundling two bead IDs into a single
  dispatch trips the per-bead-worktree Stop-hook, and the agent works around it by
  symlinking the expected paths — a hook-gaming smell that buries the real report.
  If two beads are truly coupled, still give the agent one ID and note the other
  rides with it.
- **Beads daemon can wedge on writes.** Symptoms: `bd create` / `bd dep add` time
  out on `.beads/bd.sock`; "database is locked"; "Database out of sync
  with JSONL" (that message names a JSONL file that 1.2.2 does not generate —
  it is a stale string in bd, not evidence the file went missing). Causes: a stale long-lived `bd` daemon, plus timed-out `bd create`
  commands that linger as zombies holding the DB lock. Recovery: `pgrep -x bd`
  then kill the strays (`kill -9` is safe — Dolt rolls back incomplete
  transactions; note there is NO JSONL fallback, the Dolt DB IS the store), then
  `rm -f .beads/bd.sock .beads/bd.sock.startlock`, then
  `bd --no-daemon sync --import-only`. Prefer `bd --no-daemon` for writes while
  flaky. **A supervisor gaming the Stop-hook's bead-comment check is a symptom —
  fix the daemon, don't game the hook.**
- **The auto-mode classifier blocks permission-escalating edits** (hooks,
  settings.json, `.cargo` config that grants permissions) EVEN with user
  pre-approval. Outside auto mode, with manual approval, they go through normally
  — so either hand the change to Andreas or ask him to switch modes. Don't retry
  against the classifier.
- **Worktrees inherit the MAIN checkout's `.cargo/config.toml`.** Worktrees live
  at `.worktrees/` inside the main checkout, so cargo walks up and finds main's
  config, not the worktree's. Build-config changes (`.cargo/config.toml`,
  `rust-toolchain.toml`, workspace target) are therefore **structurally
  unverifiable in a worktree** — they look broken there no matter how correct
  they are (typical symptom: `can't find crate for std`). Merge to `main`, then
  verify. Corollary: before contradicting a supervisor's "tests pass", check that
  your own reproduction isn't the thing that's broken.
- **Review agent branches against the MERGE BASE, not `main`.**
  `git diff main..branch` renders every commit main gained since the branch was
  cut as if the branch were reverting it — phantom deleted files and wild line
  counts. Use `MB=$(git merge-base main "$B"); git diff --stat "$MB".."$B"` (or
  three-dot `main...branch`). Then check `git diff --name-only` against the bead's
  stated scope, which two-dot noise otherwise buries.
- **beads is on 1.2.2 (Homebrew).** `bd comment` is REMOVED as of 1.0.0 — always
  `bd comments add`. Embedded-mode Dolt rejects hyphens in the database name
  (`.beads/metadata.json` -> `dolt_database` must use underscores, even though the
  issue *prefix* may contain hyphens). The issue prefix lives in the DATABASE, not
  `config.yaml`, so `bd create` can fail with "issue_prefix config is missing"
  while the yaml looks fine; `bd init --prefix <p>` is the fix, and
  `bd init --reinit-local` / `rm -rf .beads && bd init --prefix <p>` is the
  recovery. `bd create --id <explicit-id>` can restore a lost bead under its
  original ID. **Never upgrade bd while a supervisor is dispatched** — it can
  destroy the board and leave an agent working against a bead ID that no longer
  exists.
- **Run a full-tree audit before any history-rewriting commit.** A source-only
  grep is not enough: the pre-squash audit caught the crate name `bhk-core`
  ("Bitwarden Hardware Key core") and the stale `.claude/agents/*.md` definitions,
  neither of which lives under `core/src` or `emulator/src`.




<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->
## Current State

- **2026-08-26 — project founded by pivot.** Repo went from a Bitwarden hardware
  key to Pico Link. Merged to `main`: the strip (9 crates + 9 core modules,
  -27,996 lines), the docs rewrite (new roadmap, README, this file), removal of
  all ESP32/xtensa build config, a doc-comment sweep, the `bhk-core` ->
  `pico-link-core` crate rename, hook fixes, and agent model retiering.
  `cargo build` / `cargo test` green at the repo root, 137 tests, no `--target`
  needed.
- **Epic A is NOT finished.** Remaining, in order:
  1. Bead `pico-link-dii` — rewrite `.claude/agents/*.md` and
     `.claude/skills/vision-session/SKILL.md`, which still describe the Bitwarden
     project. **Must land before the squash.**
  2. The orphan squash — one initial commit pushed to
     `github.com/coroiu/pico-link` (created, private, still empty). Repoint
     `origin`; keep the old `bitwarden-hw-key` as remote `archive`, never
     force-pushed.
  3. Outside a running session: `mv esp32-bluetooth-tx pico-link` and restart
     Claude Code there. Renaming the cwd mid-session breaks worktree
     registrations. The orchestrator's memory directory is keyed to the old path
     and must be moved to match.
- **Next real work** is Epic B (core `no_std` port, 240x240 retarget, d-pad input
  model) and Epic C1 — flash stock USBPods on a Pico 2 W, pair headphones,
  confirm LDAC, record the codec/bitrate. C1 is Andreas's task; it is the
  baseline every later firmware change is diffed against. See
  `.planning/roadmap.md`.
