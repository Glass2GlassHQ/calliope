#!/usr/bin/env bash
# publishes one sns message per incident in the canary's status.json that opened or closed since the last run
set -euo pipefail

usage="usage: canary-alert.sh <status.json> <sent-file> <sns-topic-arn>"
status_json=${1:?$usage}
sent_file=${2:?$usage}
topic=${3:?$usage}
region=$(cut -d: -f4 <<<"$topic")

# one read, the canary rewrites the file every sample
status=$(cat "$status_json")
# "opened <id>" or "closed <id>", oldest incident first
events=$(jq -r '.incidents | reverse | .[] | (if .end == null then "opened" else "closed" end) + " " + .id' <<<"$status")

if [ ! -f "$sent_file" ]; then
    # a first run marks the closed history as sent instead of mailing up to 30 days of it
    grep '^closed ' <<<"$events" >"$sent_file" || [ $? -eq 1 ]
fi

publish() {
    local event=$1 id=$2 incident subject summary
    incident=$(jq --arg id "$id" '.incidents[] | select(.id == $id)' <<<"$status")
    if [ "$event" = opened ]; then
        subject=$(jq -r '"g2g canary: \(.pipeline) is down"' <<<"$incident")
        summary=$(jq -r '"\(.pipeline) (\(.tier)) went down at \(.start | todate)."' <<<"$incident")
    else
        subject=$(jq -r '"g2g canary: \(.pipeline) is up again"' <<<"$incident")
        summary=$(jq -r '"\(.pipeline) (\(.tier)) was down from \(.start | todate) to \(.end | todate), \(.end - .start) s."' <<<"$incident")
    fi
    aws sns publish --region "$region" --topic-arn "$topic" --subject "$subject" \
        --message "$summary"$'\n\n'"$incident" </dev/null >/dev/null
}

if [ -z "$events" ]; then
    : >"$sent_file"
    exit 0
fi
while read -r event id; do
    if grep -qxF "$event $id" "$sent_file"; then
        continue
    fi
    publish "$event" "$id"
    echo "$event $id" >>"$sent_file"
done <<<"$events"

# an incident that left status.json never comes back, so its lines can go
kept=$(grep -xF -f <(printf '%s\n' "$events") "$sent_file")
printf '%s\n' "$kept" >"$sent_file.new"
mv "$sent_file.new" "$sent_file"
