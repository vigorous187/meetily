#!/bin/sh
set -eu

archive_name="sherpa-onnx-v1.13.4-osx-arm64-static-lib.tar.bz2"
archive_size="19551872"
archive_sha256="57801db2bbb786a5d343f515a38ff210b401842338bdc804fa075312d1cd2404"

if [ -z "${SHERPA_ONNX_ARCHIVE_DIR:-}" ]; then
    echo "SHERPA_ONNX_ARCHIVE_DIR must point to the verified archive directory" >&2
    exit 2
fi

archive_path="${SHERPA_ONNX_ARCHIVE_DIR}/${archive_name}"
if [ ! -f "$archive_path" ]; then
    echo "verified sherpa-onnx archive is missing: $archive_path" >&2
    exit 2
fi

actual_size=$(stat -f '%z' "$archive_path")
if [ "$actual_size" != "$archive_size" ]; then
    echo "sherpa-onnx archive size mismatch" >&2
    exit 3
fi

actual_sha256=$(shasum -a 256 "$archive_path" | awk '{print $1}')
if [ "$actual_sha256" != "$archive_sha256" ]; then
    echo "sherpa-onnx archive SHA-256 mismatch" >&2
    exit 3
fi

export CARGO_NET_OFFLINE=true
exec cargo build -p diarization-helper --release "$@"
