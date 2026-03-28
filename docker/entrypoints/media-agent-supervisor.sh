#!/bin/sh
set -eu

stop_requested=0

handle_term() {
  stop_requested=1
}

trap handle_term INT TERM

while [ "$stop_requested" -eq 0 ]; do
  /usr/local/bin/media-agent &
  child_pid=$!

  wait "$child_pid"
  exit_code=$?
  if [ "$stop_requested" -ne 0 ]; then
    exit 0
  fi

  echo "media-agent exited with code ${exit_code}, restarting in 1s" >&2
  sleep 1
done
