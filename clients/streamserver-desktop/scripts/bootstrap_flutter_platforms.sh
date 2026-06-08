#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v flutter >/dev/null 2>&1; then
  echo "flutter was not found on PATH" >&2
  exit 127
fi

flutter create --platforms=linux,macos,windows --project-name streamserver_desktop .
flutter pub get
