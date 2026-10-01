#!/usr/bin/env bash
# Pulls Polymarket Perps' *published* outputs over REST, once an hour, for the "Later"
# fidelity work (INFO.md section 12.1). The WebSocket recorder captures the live inputs;
# this captures what Polymarket says the results were, to compare against.
#
#   instruments   <out>/instruments/YYYY-MM-DD.json   market parameters, once a day
#   funding       <out>/funding/<id>.jsonl            every published hourly rate, oldest first
#   mark history  <out>/mark/<id>/YYYY-MM-DD.jsonl    1-second marks, [bucket_ms, "price"] per line
#
# Each series keeps a cursor file (the next timestamp it needs). A cursor only advances
# after its data has been written, and is replaced atomically. So a crash re-fetches at
# most one page (at-least-once; de-duplicate by timestamp when loading), and never leaves a
# gap.
#
# API behaviour this relies on (checked 2026-09-29):
# - mark-history returns up to 1,000 points oldest-first, and the newest bucket may still
#   be open. Only buckets that closed at least 10 s ago are stored.
# - funding returns up to 100 entries *newest-first*, even when given a start time, with
#   `more: true` when older ones exist. So it pages backwards using end_timestamp.
#
# Plain shell + curl + jq on purpose: no HTTP or JSON library to trust (D-006).
# Usage: pull_history.sh <out-dir>     (see `./dev record start`)
set -uo pipefail

API="https://api.perpetuals.polymarket.com/v1/info"
OUT="${1:?usage: pull_history.sh <out-dir>}"
PAUSE=0.5              # seconds between requests, to be polite
RETRIES=4              # on HTTP 429 (rate limited), retry after 5, 10, 20, 40 s
FIRST_MARK_LOOKBACK_MS=$((2 * 3600 * 1000))
CLOSED_MARGIN_MS=10000 # a mark bucket counts as closed 10 s after it ends
MAX_PAGES=50           # per series per cycle; the next cycle continues from the cursor
FIRST_FUNDING_PAGES=10000 # a series' first funding fetch walks its whole history

log() { echo "history: $*" >&2; }
now_ms() { date -u +%s%3N; }

# GET with query parameters; prints the body, or fails on anything but HTTP 200.
# Rate-limited requests (HTTP 429) are retried with exponential backoff.
get() {
    local path="$1"; shift
    local body status attempt wait=5
    for ((attempt = 0; attempt <= RETRIES; attempt++)); do
        body=$(curl -sS --max-time 30 -G "$API/$path" "$@" -w $'\n%{http_code}') || return 1
        status="${body##*$'\n'}"
        body="${body%$'\n'*}"
        sleep "$PAUSE"
        [[ "$status" == 429 ]] || break
        sleep "$wait"; wait=$(( wait * 2 ))
    done
    [[ "$status" == 200 ]] || { log "GET $path $* -> HTTP $status"; return 1; }
    printf '%s' "$body"
}

# Prints a cursor file's value, or the given default if there is none. Fails on garbage.
read_cursor() {
    local file="$1" default="$2" value
    value=$(cat "$file" 2>/dev/null) || value="$default"
    [[ "$value" =~ ^[0-9]+$ ]] || { log "bad cursor in $file: '$value'"; return 1; }
    printf '%s' "$value"
}

write_cursor() {
    printf '%s\n' "$2" > "$1.tmp" && mv "$1.tmp" "$1"
}

# An append that failed partway (disk full) leaves a partial last line. End it before the
# next append, so the retried records start on a line of their own.
end_line() {
    if [[ -s "$1" && -n $(tail -c 1 "$1") ]]; then echo >> "$1"; fi
}

pull_instruments() {
    local file="$OUT/instruments/$(date -u +%F).json"
    mkdir -p "$OUT/instruments"
    [[ -s "$file" ]] && return 0
    get instruments > "$file.tmp" && mv "$file.tmp" "$file"
}

instrument_ids() {
    jq -r '.[].instrument_id' "$(ls -1 "$OUT"/instruments/*.json | tail -n 1)"
}

pull_funding() {
    local id="$1" dir="$OUT/funding"
    local file="$dir/$id.jsonl" cursor_file="$dir/$id.cursor"
    mkdir -p "$dir"
    local start end="" body page oldest pages="$dir/$id.pages.tmp" limit=$MAX_PAGES
    [[ -e "$cursor_file" ]] || limit=$FIRST_FUNDING_PAGES
    start=$(read_cursor "$cursor_file" 0) || return 1   # no cursor yet: fetch all history
    : > "$pages"

    # Newest-first pages: walk backwards from now to `start`, collecting into a temp file.
    for ((page = 0; page < limit; page++)); do
        local args=(--data-urlencode "instrument_id=$id" --data-urlencode "start_timestamp=$start")
        [[ -n "$end" ]] && args+=(--data-urlencode "end_timestamp=$end")
        body=$(get funding "${args[@]}") || return 1
        jq -c --argjson start "$start" '.data[] | select(.timestamp >= $start)' <<<"$body" >> "$pages" || return 1
        [[ $(jq -r '.more' <<<"$body") == true ]] || break
        oldest=$(jq '[.data[].timestamp] | min // empty' <<<"$body") || return 1
        # Stop on an empty page, or once the page reaches back past `start`.
        [[ -n "$oldest" ]] && (( oldest > start )) || break
        end=$(( oldest - 1 ))
    done
    (( page < limit )) || { log "funding $id: more than $limit pages; not advancing"; return 1; }

    [[ -s "$pages" ]] || { rm -f "$pages"; return 0; }
    end_line "$file"
    jq -cs 'unique_by(.timestamp) | sort_by(.timestamp) | .[]' "$pages" >> "$file" || return 1
    write_cursor "$cursor_file" $(( $(jq -s '[.[].timestamp] | max' "$pages") + 1 ))
    rm -f "$pages"
}

pull_mark_history() {
    local id="$1" dir="$OUT/mark/$1"
    mkdir -p "$dir"
    local cursor_file="$dir/.cursor" start cutoff page body closed count
    start=$(read_cursor "$cursor_file" $(( $(now_ms) - FIRST_MARK_LOOKBACK_MS ))) || return 1
    cutoff=$(( $(now_ms) - CLOSED_MARGIN_MS ))

    for ((page = 0; page < MAX_PAGES; page++)); do
        body=$(get mark-history --data-urlencode "instrument_id=$id" \
            --data-urlencode "interval=1s" --data-urlencode "start_timestamp=$start") || return 1
        # Keep only buckets that have closed; the next cycle picks up the rest.
        closed=$(jq -c --argjson cut "$cutoff" '[.data[] | select(.[0] + 1000 <= $cut)]' <<<"$body") || return 1
        count=$(jq 'length' <<<"$closed")
        (( count > 0 )) || break
        # File each point under its own UTC day. The cursor moves only if the append worked.
        # Only the newest day file can have been left mid-line by a failed append.
        end_line "$(ls -1 "$dir"/*.jsonl 2>/dev/null | tail -n 1)"
        jq -r '.[] | "\(.[0] / 1000 | floor | strftime("%Y-%m-%d"))\t\(tojson)"' <<<"$closed" \
            | awk -F'\t' -v dir="$dir" '{ print $2 >> (dir "/" $1 ".jsonl") }' || return 1
        start=$(( $(jq '.[-1][0]' <<<"$closed") + 1000 ))
        write_cursor "$cursor_file" "$start" || return 1
        # Stop when the server has nothing more, or when we reached still-open buckets.
        [[ $(jq -r '.more' <<<"$body") == true && $count == $(jq '.data | length' <<<"$body") ]] || break
    done
}

log "writing to $OUT"
while true; do
    cycle_start=$(date +%s)
    if pull_instruments; then
        for id in $(instrument_ids); do
            pull_funding "$id" || log "funding $id failed; will retry next cycle"
            pull_mark_history "$id" || log "mark history $id failed; will retry next cycle"
        done
        log "cycle done in $(( $(date +%s) - cycle_start ))s"
    else
        log "instrument list unavailable; retrying next cycle"
    fi
    # Next cycle five minutes past the next hour, after that hour's funding settles.
    now=$(date +%s)
    sleep $(( 3600 - now % 3600 + 300 ))
done
