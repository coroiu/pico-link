---
name: session-resume
description: Pick up work at the start of a session - load the handoff, verify it still matches reality, and propose the next dispatch. Use when the user says session-resume, "let's continue", "where were we", "what's next", or opens a fresh context expecting work to carry on. The companion to session-close. Runs on the main thread; do NOT dispatch it to a subagent.
---

# Session Resume

The companion to `session-close`. That skill wrote a handoff for a competent
stranger; you are that stranger. Your job is to load it, **check it is still
true**, and propose one next action — not to start working immediately.

Work through all five steps, then report. Keep it cheap: this is orientation,
not investigation. If it takes more than a handful of tool calls, you are doing
step 4's job instead of step 4's dispatch.

## 1. Read the handoff first

Start with the memory index and the handoff memory it points to. That is where
the last session put the next task, and the constraints a fresh agent would
otherwise cheerfully violate.

**Treat memory as dated, not authoritative.** It records what was true when
written. If it names a file, function, flag or bead, verify that still exists
before recommending anything built on it. A confidently-followed stale memory
is worse than no memory.

## 2. Check the board against it

```bash
bd ready
bd list --json --status open     # never bare `bd list` — it truncates silently
bd show <the bead the handoff names>
```

Confirm the named next bead still exists, is still open, and is still
unblocked. If the handoff and the board disagree, **the board wins** — then say
so in your report, because it means something moved after the handoff was
written.

Read that bead's comments before anything else. Prior sessions bank measured
evidence, dead hypotheses and explicit "do not re-derive this" notes there.
Re-deriving them is the single most expensive mistake available at this point.

## 3. Verify the workspace matches the story

```bash
git status --short
git log --oneline -5
git worktree list
```

Look for the gap between what the handoff claims and what is actually here:
uncommitted work, a worktree left behind, a branch that never merged, a bead
marked `in_progress` with nothing to show. To check whether a branch reached
the remote, compare hashes with `git ls-remote` — a missing upstream or a stale
`git branch -r` proves nothing.

Also skim the newest ADR in `.planning/decisions/`. When architecture has
changed recently, older docs and older beads describe a world that no longer
exists, and following them wastes a session.

## 4. Propose one next action

Name **one** thing to do next and why, with the bead ID. Prefer what the
handoff named unless step 2 or 3 contradicted it — if it did, say what changed.

Before dispatching, restate for the implementer:
- what is already banked and must not be re-investigated, with where it lives
- any constraint that must not be quietly undone
- any spend cap or stopping rule still in force from the previous session

If nothing is obviously next, say that plainly and list the decisions blocking
the choice. Do not manufacture a task to look useful.

## 5. Report, then wait

Lead with: where things stand in two or three lines, and the proposed next
action. Then flag anything that needs the user's decision.

**Do not start the work in the same turn as proposing it** unless the user's
message already told you to continue. Resuming is orientation; the user may
have arrived with different priorities than the last session's handoff assumed.

## Rules

- **Never invent progress.** If the last session did not finish something, say
  it did not. Check the bead, do not infer from an optimistic-sounding summary.
- **Do not re-run design work already recorded in a bead.** If the description
  carries a full design, the implementer needs none from you.
- Keep orientation small. Large outputs read on the main thread are paid for
  for the rest of the session — see the context discipline in CLAUDE.md.
