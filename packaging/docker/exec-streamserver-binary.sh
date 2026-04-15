#!/bin/sh
set -eu

binary_name="${STREAMSERVER_BINARY_NAME:-streamserver-binary}"
binary_path="${STREAMSERVER_BINARY_PATH:-}"

if [ -z "${binary_path}" ]; then
  echo "missing STREAMSERVER_BINARY_PATH for ${binary_name}" >&2
  exit 1
fi

if [ ! -e "${binary_path}" ]; then
  echo "expected host-mounted ${binary_name} at ${binary_path}, but it does not exist" >&2
  exit 1
fi

if [ ! -x "${binary_path}" ]; then
  echo "expected executable ${binary_name} at ${binary_path}, but execute permission is missing" >&2
  exit 1
fi

exec "${binary_path}" "$@"
