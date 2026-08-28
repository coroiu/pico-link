#!/bin/bash
#
# PostToolUse:Bash (async) - Capture knowledge from bd comment commands
#
# Detects a REAL invocation of:  bd comments add {BEAD_ID} "LEARNED: ..."
# and appends a knowledge entry to .beads/knowledge.jsonl
#
# Three gates keep junk out (bead pico-link-iyf). Earlier versions matched on
# the raw command TEXT, so any grep, heredoc, test harness or shell function
# whose source merely CONTAINED the invocation produced a bogus entry:
#
#   1. The tool call must have SUCCEEDED. A failed Bash call arrives with a
#      string tool_response ("Error: Exit code 1..."), a successful one with an
#      object {stdout,stderr,interrupted,...}.
#   2. The invocation must sit at a COMMAND POSITION - first token, or after a
#      shell separator - in a shell-tokenized reading of the command. Quoted
#      occurrences collapse into a single token and no longer match; heredoc
#      bodies are stripped; commands that define a shell function are refused
#      outright, since their body is text, not an executed command.
#   3. The extracted bead ID must RESOLVE on the board. This alone would have
#      rejected the three bogus pico-link-abc entries of 2026-08-26.
#
# A single Bash call may contain SEVERAL real invocations - agents are told to
# bank findings as they go and naturally batch them. Every one of them is
# captured, in source order (bead pico-link-lfm); an earlier version kept only
# the last. Gate 3 runs per invocation, with resolved bead IDs memoised so a
# batch that names the same bead repeatedly costs one board lookup, and the
# whole loop stops if it approaches the hook timeout.
#
# Every gate fails closed: when in doubt, capture nothing.
#

INPUT=$(cat)
TOOL_NAME=$(echo "$INPUT" | jq -r '.tool_name // empty')

# Only process Bash tool
[[ "$TOOL_NAME" != "Bash" ]] && exit 0

# --- Gate 1: prefer structured payload fields; require the command to have run
# and succeeded. A string tool_response is an error report; an interrupted call
# never completed. A missing tool_response (older payload shapes) is tolerated.
RESP_TYPE=$(echo "$INPUT" | jq -r '.tool_response | type' 2>/dev/null)
case "$RESP_TYPE" in
  object|null|"") ;;
  *) exit 0 ;;
esac
if [[ "$RESP_TYPE" == "object" ]]; then
  echo "$INPUT" | jq -e '.tool_response.interrupted == true' >/dev/null 2>&1 && exit 0
  echo "$INPUT" | jq -e '.tool_response.is_error == true' >/dev/null 2>&1 && exit 0
fi

# --- Gate 2: shell-aware detection + extraction. Emits {"bead":..,"body":..}
# on stdout, or nothing (non-zero) when no real invocation is present.
command -v python3 >/dev/null 2>&1 || exit 0
PARSED=$(CAPTURE_INPUT="$INPUT" python3 <<'PY'
import json, os, re, shlex, sys

try:
    data = json.loads(os.environ["CAPTURE_INPUT"])
except Exception:
    sys.exit(1)
cmd = ((data.get("tool_input") or {}).get("command") or "")
if not cmd.strip():
    sys.exit(1)

# Strip heredoc bodies: their lines are data handed to another program, not
# commands, even though they start at a line boundary.
lines = cmd.split("\n")
kept, i = [], 0
while i < len(lines):
    line = lines[i]
    kept.append(line)
    for m in re.finditer(r"<<[-~]?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1", line):
        term = m.group(2)
        i += 1
        while i < len(lines) and lines[i].strip() != term:
            i += 1
    i += 1
stripped = "\n".join(kept)

# A command that DEFINES a shell function is refused outright: the body is
# source text at definition time, and we cannot tell whether it ever ran.
if re.search(r"(?:^|[;&|(){}\n])\s*(?:function\s+)?[A-Za-z_][A-Za-z0-9_]*\s*\(\s*\)\s*\{", stripped, re.S) \
   or re.search(r"(?:^|[;&|\n])\s*function\s+[A-Za-z_][A-Za-z0-9_]*", stripped):
    sys.exit(1)

# Newlines separate commands just as `;` does, but shlex treats them as plain
# whitespace - so make them explicit, outside quotes only (a newline inside a
# quoted string is data), and fold line continuations into a space.
def normalize_newlines(text):
    out, i, sq, dq = [], 0, False, False
    while i < len(text):
        c = text[i]
        if c == "\\" and not sq:
            if i + 1 < len(text) and text[i + 1] == "\n" and not dq:
                out.append(" ")
                i += 2
                continue
            out.append(c)
            if i + 1 < len(text):
                out.append(text[i + 1])
            i += 2
            continue
        if c == "'" and not dq:
            sq = not sq
        elif c == '"' and not sq:
            dq = not dq
        out.append(" ; " if (c == "\n" and not sq and not dq) else c)
        i += 1
    return "".join(out)

stripped = normalize_newlines(stripped)

try:
    lex = shlex.shlex(stripped, posix=True, punctuation_chars=True)
    lex.whitespace_split = True
    toks = list(lex)
except ValueError:
    # Unbalanced quotes - we cannot reason about it, so we do not capture.
    sys.exit(1)

SEP = {";", ";;", "&&", "||", "|", "|&", "&", "(", ")", "{", "}",
       "then", "do", "else", "elif", "!"}

found = []
for i, t in enumerate(toks):
    if t.rsplit("/", 1)[-1] != "bd":
        continue
    if i != 0 and toks[i - 1] not in SEP:
        continue
    j = i + 1
    while j < len(toks) and toks[j].startswith("-"):
        j += 1
    if j >= len(toks) or toks[j] not in ("comment", "comments"):
        continue
    j += 1
    if j < len(toks) and toks[j] == "add":
        j += 1
    while j < len(toks) and toks[j].startswith("-"):
        j += 1
    if j >= len(toks):
        continue
    bead = toks[j]
    j += 1
    if not re.fullmatch(r"[A-Za-z0-9._-]+", bead):
        continue
    while j < len(toks) and toks[j].startswith("-"):
        j += 1
    if j >= len(toks):
        continue
    body = toks[j]
    if "LEARNED:" not in body:
        continue
    found.append({"bead": bead, "body": body[:4096]})

if not found:
    sys.exit(1)
# Cap runaway calls: a hook has a hard time budget and no realistic bank-as-you-go
# batch is this large.
print(json.dumps(found[:20]))
PY
) || exit 0
[[ -z "$PARSED" ]] && exit 0

# --- Gate 3 prerequisites: the bead must exist on the board. `bd show` exits 0
# either way, so resolve through --json. Guarded by a watchdog: the beads daemon
# can wedge, and a hook must never hang the session.
BD_BIN=$(command -v bd 2>/dev/null)
[[ -z "$BD_BIN" && -x /opt/homebrew/bin/bd ]] && BD_BIN=/opt/homebrew/bin/bd
[[ -z "$BD_BIN" ]] && exit 0

# Memoised bead resolution. macOS ships bash 3.2, which has no associative
# arrays, so the cache is a newline-delimited "query<TAB>resolved" string. A
# miss is cached as an empty resolution, so a batch that repeats the same bogus
# ID does not pay for it twice either. The result is returned in the global
# RESOLVED_OUT rather than on stdout: command substitution would run the
# function in a subshell and throw the cache away.
BD_CACHE=""
RESOLVED_OUT=""
resolve_bead() {
  local q="$1" key val out r wd tmpf
  RESOLVED_OUT=""
  while IFS=$'\t' read -r key val; do
    if [[ -n "$key" && "$key" == "$q" ]]; then
      RESOLVED_OUT="$val"
      return 0
    fi
  done <<< "$BD_CACHE"

  # The watchdog never outlives the remaining budget: a wedged daemon on the
  # last permitted lookup must not push the hook past its own 10s timeout.
  wd=$(( BUDGET - SECONDS ))
  [[ "$wd" -lt 1 ]] && wd=1
  [[ "$wd" -gt 5 ]] && wd=5
  # The lookup writes to a temp FILE, not into a command substitution. Reading
  # it through a substitution pipe made the watchdog useless: killing bd left
  # any grandchild it had forked holding the pipe open, so the substitution
  # blocked for as long as the grandchild lived (measured: 60s against a bd
  # that just sleeps, on the pre-fix hook too). With a file, `wait` returns the
  # moment the direct child dies and the watchdog actually bounds the call.
  tmpf=$(mktemp "${TMPDIR:-/tmp}/bd-capture.XXXXXX" 2>/dev/null) || return 0
  "$BD_BIN" show "$q" --json >"$tmpf" 2>/dev/null &
  BPID=$!
  ( sleep "$wd"; kill -9 "$BPID" ) >/dev/null 2>&1 &
  WPID=$!
  wait "$BPID" 2>/dev/null
  kill -9 "$WPID" >/dev/null 2>&1
  wait "$WPID" 2>/dev/null
  out=$(cat "$tmpf" 2>/dev/null)
  rm -f "$tmpf"
  r=$(echo "$out" | jq -r 'if type=="array" then (.[0].id // empty) else empty end' 2>/dev/null)
  if [[ -n "$r" ]] && [[ "$(echo "$r" | tr '[:upper:]' '[:lower:]')" == "$(echo "$q" | tr '[:upper:]' '[:lower:]')" ]]; then
    RESOLVED_OUT="$r"
  fi
  BD_CACHE="${BD_CACHE}${q}"$'\t'"${RESOLVED_OUT}"$'\n'
}

# Resolve memory directory once; entries are appended in source order.
MEMORY_DIR="${CLAUDE_PROJECT_DIR:-.}/.beads"
mkdir -p "$MEMORY_DIR"
KNOWLEDGE_FILE="$MEMORY_DIR/knowledge.jsonl"

# Source agent: same for every entry in the call.
SOURCE="orchestrator"
CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if echo "$CWD" | grep -q '\.worktrees/'; then
  # Inside a worktree = supervisor is running
  SOURCE="supervisor"
fi

# The hook's own timeout is 10s. Each uncached board lookup costs ~0.9s, so a
# large batch of distinct beads could otherwise run past it and be killed
# mid-write; stop early instead and keep what was already captured. Repeated
# bead IDs are free, so this only bites on many DISTINCT beads in one call.
# BUDGET also bounds the per-lookup watchdog, making the whole loop hard-capped
# at roughly BUDGET seconds rather than BUDGET plus a wedged lookup.
SECONDS=0
BUDGET=7

while IFS= read -r ITEM; do
  [[ -z "$ITEM" ]] && continue
  if [[ "$SECONDS" -ge "$BUDGET" ]]; then
    break
  fi

  BEAD_ID=$(echo "$ITEM" | jq -r '.bead // empty')
  COMMENT_BODY=$(echo "$ITEM" | jq -r '.body // empty')
  [[ -z "$BEAD_ID" || -z "$COMMENT_BODY" ]] && continue

  # --- Gate 3, per invocation.
  resolve_bead "$BEAD_ID"
  [[ -z "$RESOLVED_OUT" ]] && continue
  BEAD_ID="$RESOLVED_OUT"

  # Determine type and extract content (voluntary LEARNED only)
  TYPE=""
  CONTENT=""
  if echo "$COMMENT_BODY" | grep -q "LEARNED:"; then
    TYPE="learned"
    CONTENT=$(echo "$COMMENT_BODY" | sed 's/.*LEARNED:[[:space:]]*//' | head -c 2048)
  fi
  [[ -z "$TYPE" || -z "$CONTENT" ]] && continue

  # Generate key from content (type + slugified first 60 chars)
  SLUG=$(echo "$CONTENT" | head -c 60 | tr '[:upper:]' '[:lower:]' | tr -cs 'a-z0-9' '-' | sed 's/^-//;s/-$//')
  KEY="${TYPE}-${SLUG}"

  # Build tags array - start with type tag
  TAGS_ARRAY=("$TYPE")

  # Scan content for known tech keywords and add matching tags
  for tag in swift swiftui appkit menubar api security test database \
             networking ui layout performance crash bug fix workaround \
             gotcha pattern convention architecture auth middleware \
             async concurrency model protocol adapter scanner engine; do
    if echo "$CONTENT" | grep -qi "$tag"; then
      TAGS_ARRAY+=("$tag")
    fi
  done

  # Convert tags array to JSON
  TAGS_JSON=$(printf '%s\n' "${TAGS_ARRAY[@]}" | jq -R . | jq -s .)

  # Get timestamp
  TS=$(date +%s)

  # Build JSON entry with proper escaping
  ENTRY=$(jq -cn \
    --arg key "$KEY" \
    --arg type "$TYPE" \
    --arg content "$CONTENT" \
    --arg source "$SOURCE" \
    --argjson tags "$TAGS_JSON" \
    --argjson ts "$TS" \
    --arg bead "$BEAD_ID" \
    '{key: $key, type: $type, content: $content, source: $source, tags: $tags, ts: $ts, bead: $bead}')

  # Validate JSON
  [[ -z "$ENTRY" ]] && continue
  echo "$ENTRY" | jq . >/dev/null 2>&1 || continue

  # Append entry
  echo "$ENTRY" >> "$KNOWLEDGE_FILE"
done < <(echo "$PARSED" | jq -c '.[]' 2>/dev/null)

# Rotation: archive oldest 500 when file exceeds 1000 lines. Once per call,
# after every entry has been appended.
[[ -f "$KNOWLEDGE_FILE" ]] || exit 0
LINE_COUNT=$(wc -l < "$KNOWLEDGE_FILE" 2>/dev/null | tr -d ' ')
if [[ "$LINE_COUNT" -gt 1000 ]]; then
  ARCHIVE_FILE="$MEMORY_DIR/knowledge.archive.jsonl"
  head -500 "$KNOWLEDGE_FILE" >> "$ARCHIVE_FILE"
  tail -n +501 "$KNOWLEDGE_FILE" > "$KNOWLEDGE_FILE.tmp"
  mv "$KNOWLEDGE_FILE.tmp" "$KNOWLEDGE_FILE"
fi

exit 0
