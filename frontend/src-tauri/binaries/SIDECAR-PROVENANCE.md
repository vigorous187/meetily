# Native sidecar provenance

These Apple Silicon executables are reviewed release inputs. They are bundled
beside the main Meetily executable and are never downloaded or selected from
`PATH` at runtime.

The application build verifies each input's exact size and SHA-256, rejects
symlinks, non-regular files, and non-executable files, and requires a thin
`arm64` Mach-O with minimum macOS 11.0 and Apple system-only dynamic
dependencies. A missing or changed artifact fails the build before packaging.

| File | Size (bytes) | SHA-256 | Minimum macOS |
| --- | ---: | --- | --- |
| `llama-helper-aarch64-apple-darwin` | 5,190,784 | `68a72d9a4edf64c8284f79e6379e2f0dad5b2d94118591b025f070c8e5fa0daf` | 11.0 |
| `diarization-helper-aarch64-apple-darwin` | 23,505,600 | `78ec589bdd38c8d041d6cf5c49c852022c6d996bdf10ef106bb8376040038001` | 11.0 |

The release script applies an entitlement-free ad-hoc hardened-runtime
signature after copying the reviewed diarization input. The reviewed packaged
form is 23,369,056 bytes with SHA-256
`03d245d0c69d60b6cae1f1b8e41d18bb7a1d1cda073d831735f882186a3f6773`.
Runtime verification accepts only these two exact pre-sign and packaged forms.

The packaged llama helper is 5,160,736 bytes with SHA-256
`eebae2a1e27acd0258a89630a889e470cdd6a8c896e73359f0d871595df3d296`.

Both files are thin `arm64` Mach-O executables and link only Apple system
libraries and frameworks. The diarization helper statically embeds its
reviewed sherpa-onnx/ONNX Runtime native dependencies.

## Rebuilding

- Build `llama-helper` from the workspace lockfile with a fresh target
  directory, `MACOSX_DEPLOYMENT_TARGET=11.0`, `--locked`, and `--release`.
- Build `diarization-helper` with `diarization-helper/build-offline.sh`. That
  wrapper verifies the pinned sherpa-onnx archive before forcing Cargo offline.
- Re-run the workspace tests and release linkage checks, then update this file
  and the corresponding runtime integrity constants only after reviewing the
  newly produced artifact.

FFmpeg has separate source, signature, configuration, and licensing records in
`FFMPEG-PROVENANCE.md` and `COPYING.LGPLv2.1`.
