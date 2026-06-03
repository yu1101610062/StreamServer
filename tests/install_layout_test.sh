#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
tmp_dir="$(mktemp -d)"
functions_file="${tmp_dir}/install-functions.sh"
trap 'chmod -R u+w "${tmp_dir}" 2>/dev/null || true; rm -rf "${tmp_dir}"' EXIT

# Load the installer functions without executing the interactive installer entrypoint.
awk '
  /^log\(\)/ { emit = 1 }
  /^emit_manual_start_hint\(\)/ { emit = 0 }
  emit { print }
' "${REPO_ROOT}/packaging/offline/install.sh" >"${functions_file}"
# shellcheck disable=SC1090
. "${functions_file}"

install_dir="${tmp_dir}/install"
readonly_www_dir="${tmp_dir}/nfs/www"
readonly_output_dir="${tmp_dir}/nfs/output"

mkdir -p "${install_dir}" "${readonly_www_dir}" "${readonly_output_dir}"
chmod 555 "${readonly_www_dir}" "${readonly_output_dir}"

ZLM_WWW_MOUNT_HOST_DIR="${readonly_www_dir}"
ZLM_OUTPUT_MOUNT_HOST_DIR="${readonly_output_dir}"

prepare_worker_layout "${install_dir}"

[ -d "${install_dir}/data/media/work" ]
[ -d "${install_dir}/data/media/logs" ]
[ ! -e "${readonly_output_dir}/mp4" ]
[ ! -e "${readonly_output_dir}/hls" ]

mounted_www_dir="${tmp_dir}/mounted-www"
install_with_mounted_www="${tmp_dir}/install-with-mounted-www"
mkdir -p "${mounted_www_dir}"
chmod 555 "${mounted_www_dir}"

ZLM_WWW_MOUNT_HOST_DIR="${mounted_www_dir}"
ZLM_OUTPUT_MOUNT_HOST_DIR="${mounted_www_dir}/output"

prepare_worker_layout "${install_with_mounted_www}"

[ ! -e "${mounted_www_dir}/output" ]

dangling_www_link="${tmp_dir}/dangling-www"
second_install_dir="${tmp_dir}/install-with-link"
ln -s "${tmp_dir}/missing-www-target" "${dangling_www_link}"

ZLM_WWW_MOUNT_HOST_DIR="${dangling_www_link}"
ZLM_OUTPUT_MOUNT_HOST_DIR="${readonly_output_dir}"

prepare_worker_layout "${second_install_dir}"

[ -L "${dangling_www_link}" ]

unset ZLM_WWW_MOUNT_HOST_DIR
unset ZLM_OUTPUT_MOUNT_HOST_DIR
default_install_dir="${tmp_dir}/default-install"

prepare_worker_layout "${default_install_dir}"

[ -d "${default_install_dir}/data/zlm/www/output/mp4" ]
[ -d "${default_install_dir}/data/zlm/www/output/hls" ]
