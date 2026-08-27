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

found = None
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
    found = {"bead": bead, "body": body[:4096]}

if found is None:
    sys.exit(1)
print(json.dumps(found))
PY
) || exit 0
[[ -z "$PARSED" ]] && exit 0

BEAD_ID=$(echo "$PARSED" | jq -r '.bead // empty')
COMMENT_BODY=$(echo "$PARSED" | jq -r '.body // empty')
[[ -z "$BEAD_ID" || -z "$COMMENT_BODY" ]] && exit 0

# --- Gate 3: the bead must exist on the board. `bd show` exits 0 either way,
# so resolve through --json. Guarded by a watchdog: the beads daemon can wedge,
# and a hook must never hang the session.
BD_BIN=$(command -v bd 2>/dev/null)
[[ -z "$BD_BIN" && -x /opt/homebrew/bin/bd ]] && BD_BIN=/opt/homebrew/bin/bd
[[ -z "$BD_BIN" ]] && exit 0

BD_OUT=$( { "$BD_BIN" show "$BEAD_ID" --json 2>/dev/null & BPID=$!
            { sleep 5; kill -9 "$BPID"; } >/dev/null 2>&1 & WPID=$!
            wait "$BPID"
            kill -9 "$WPID" >/dev/null 2>&1; } )
RESOLVED=$(echo "$BD_OUT" | jq -r 'if type=="array" then (.[0].id // empty) else empty end' 2>/dev/null)
[[ -z "$RESOLVED" ]] && exit 0
[[ "$(echo "$RESOLVED" | tr '[:upper:]' '[:lower:]')" != "$(echo "$BEAD_ID" | tr '[:upper:]' '[:lower:]')" ]] && exit 0
BEAD_ID="$RESOLVED"

# Determine type and extract content (voluntary LEARNED only)
TYPE=""
CONTENT=""
if echo "$COMMENT_BODY" | grep -q "LEARNED:"; then
  TYPE="learned"
  CONTENT=$(echo "$COMMENT_BODY" | sed 's/.*LEARNED:[[:space:]]*//' | head -c 2048)
fi

[[ -z "$TYPE" || -z "$CONTENT" ]] && exit 0

# Generate key from content (type + slugified first 60 chars)
SLUG=$(echo "$CONTENT" | head -c 60 | tr '[:upper:]' '[:lower:]' | tr -cs 'a-z0-9' '-' | sed 's/^-//;s/-$//')
KEY="${TYPE}-${SLUG}"

# Detect source agent from CWD or transcript context
SOURCE="orchestrator"
CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if echo "$CWD" | grep -q '\.worktrees/'; then
  # Inside a worktree = supervisor is running
  SOURCE="supervisor"
fi

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
[[ -z "$ENTRY" ]] && exit 0
echo "$ENTRY" | jq . >/dev/null 2>&1 || exit 0

# Resolve memory directory
MEMORY_DIR="${CLAUDE_PROJECT_DIR:-.}/.beads"
mkdir -p "$MEMORY_DIR"
KNOWLEDGE_FILE="$MEMORY_DIR/knowledge.jsonl"

# Append entry
echo "$ENTRY" >> "$KNOWLEDGE_FILE"

# Rotation: archive oldest 500 when file exceeds 1000 lines
LINE_COUNT=$(wc -l < "$KNOWLEDGE_FILE" 2>/dev/null | tr -d ' ')
if [[ "$LINE_COUNT" -gt 1000 ]]; then
  ARCHIVE_FILE="$MEMORY_DIR/knowledge.archive.jsonl"
  head -500 "$KNOWLEDGE_FILE" >> "$ARCHIVE_FILE"
  tail -n +501 "$KNOWLEDGE_FILE" > "$KNOWLEDGE_FILE.tmp"
  mv "$KNOWLEDGE_FILE.tmp" "$KNOWLEDGE_FILE"
fi

exit 0
