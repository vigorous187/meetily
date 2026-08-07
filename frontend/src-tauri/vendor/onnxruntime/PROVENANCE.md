# Static ONNX Runtime provenance

Meetily links ONNX Runtime 1.22.0 statically for its local Parakeet and semantic
search models. The reviewed Apple Silicon archive is stored in this directory
so a fresh release build is self-contained and does not invoke the `ort-sys`
binary downloader.

## Reviewed artifact

- Upstream distribution record: `ort-sys` 2.0.0-rc.10 `dist.txt`
- Distribution URL recorded by the pinned crate: `https://cdn.pyke.io/0/pyke:ort-rs/ms@1.22.0/aarch64-apple-darwin.tgz`
- Distribution SHA-256 recorded and enforced by the pinned crate: `00FBFD6F08BAC2A4E28C66723AF900D58D1B4B1C73EFBA6290637CD3019883D5`
- Extracted file: `aarch64-apple-darwin/lib/libonnxruntime.a`
- Extracted size: `71,060,144` bytes
- Extracted SHA-256: `e5c83560aa9e88afa39d9dca9fb5f5a767e28adb5458d1c36fe0357131b6af8b`
- Architecture: thin `arm64` static archive
- Required exported C API symbol: `OrtGetApiBase`

The archive was copied from the existing `ort-sys` content-addressed cache at
the exact distribution-hash directory above. No new network artifact was used.
The application build independently verifies the extracted size, SHA-256,
architecture, symbol, configured library directory, and disabled downloader.

The workspace Cargo configuration pins `ORT_LIB_LOCATION` to this directory
and forces `ORT_SKIP_DOWNLOAD=true`. This also avoids the `ort-sys` offline-mode
empty linker-search-path bug.
