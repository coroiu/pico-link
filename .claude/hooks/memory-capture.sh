#!/bin/bash
#
# PostToolUse:Bash (async) - Capture knowledge from bd comment commands
#
# Detects: bd comment {BEAD_ID} "LEARNED: ..."
# Extracts knowledge entries into .beads/knowledge.jsonl
#

INPUT=$(cat)
TOOL_NAME=$(echo "$INPUT" | jq -r '.tool_name // empty')

# Only process Bash tool
[[ "$TOOL_NAME" != "Bash" ]] && exit 0

# Extract the command that was executed
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')
[[ -z "$COMMAND" ]] && exit 0

# Only process bd comment commands containing knowledge markers
echo "$COMMAND" | grep -qE 'bd[[:space:]]+comments?[[:space:]]+(add[[:space:]]+)?' || exit 0
echo "$COMMAND" | grep -qE 'LEARNED:' || exit 0

# Flatten to a single line first: COMMAND may be a compound/multi-line shell
# script (e.g. "cd ... && bd comments add ..."), and sed applies substitutions
# per-line, so a multi-line COMMAND left non-matching lines untouched and
# corrupted the extraction below.
COMMAND_FLAT=$(echo "$COMMAND" | tr '\n' ' ')

# Extract BEAD_ID (last token of the matched "bd comment(s) [add] ID" span)
BEAD_ID=$(echo "$COMMAND_FLAT" | grep -oE 'bd[[:space:]]+comments?[[:space:]]+(add[[:space:]]+)?[A-Za-z0-9._-]+' | tail -1 | awk '{print $NF}')
[[ -z "$BEAD_ID" || "$BEAD_ID" == "$COMMAND_FLAT" ]] && exit 0

# Which quote character opens the comment body (bd accepts either).
QUOTE_CHAR=$(echo "$COMMAND_FLAT" | sed -E 's/.*bd[[:space:]]+comments?[[:space:]]+(add[[:space:]]+)?[A-Za-z0-9._-]+[[:space:]]+(["'\'']).*/\2/')
[[ -z "$QUOTE_CHAR" || "$QUOTE_CHAR" == "$COMMAND_FLAT" ]] && exit 0

# Extract the comment body: strip everything up to and including the opening
# quote, then cut at the FIRST matching closing quote rather than anchoring at
# end-of-string. After flattening, the body no longer ends the string - a
# trailing `&& next-cmd` or `| tail -2` follows it - so an end-anchored strip
# left that trailing shell glued onto the body. Splitting on the opening
# quote char instead stops at its first recurrence (the real close) and,
# since it only splits on that one character, an apostrophe inside a
# double-quoted body (or vice versa) doesn't trigger an early cut.
COMMENT_BODY=$(echo "$COMMAND_FLAT" | sed -E 's/.*bd[[:space:]]+comments?[[:space:]]+(add[[:space:]]+)?[A-Za-z0-9._-]+[[:space:]]+["'\'']//' | awk -F"$QUOTE_CHAR" '{print $1}' | head -c 4096)
[[ -z "$COMMENT_BODY" ]] && exit 0

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
