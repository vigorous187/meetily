# Meetily diarization helper

This one-shot helper isolates sherpa-onnx and its ONNX Runtime from Meetily's main process.
It performs no network access and accepts only the two reviewed model files whose sizes and
SHA-256 values are compiled into the executable.

## Verified offline build

The `sherpa-onnx-sys` crate otherwise downloads its native archive without checking a digest.
Always build through the wrapper with a pre-verified archive:

```sh
export SHERPA_ONNX_ARCHIVE_DIR=/absolute/path/to/verified/archive-directory
./diarization-helper/build-offline.sh
```

The directory must contain exactly:

`sherpa-onnx-v1.13.4-osx-arm64-static-lib.tar.bz2`

- Size: `19,551,872` bytes
- SHA-256: `57801db2bbb786a5d343f515a38ff210b401842338bdc804fa075312d1cd2404`

The wrapper verifies both properties and sets `CARGO_NET_OFFLINE=true` before invoking Cargo.

## CLI

```text
diarization-helper \
  --audio /absolute/path/system-audio.wav \
  --segmentation-model /absolute/path/pyannote-segmentation-model.onnx \
  --embedding-model /absolute/path/3dspeaker-embedding-model.onnx \
  --num-clusters -1
```

The WAV must be mono, 16-bit PCM, and 16 kHz. Success writes one JSON object to stdout:

```json
{"version":1,"turns":[{"start_ms":0,"end_ms":1000,"cluster_index":0}]}
```

Errors are written to stderr and return a nonzero exit status. The parent process owns timeout
and cancellation by terminating the helper.
