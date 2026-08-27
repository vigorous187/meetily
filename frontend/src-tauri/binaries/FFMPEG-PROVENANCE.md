# FFmpeg provenance

Meetily bundles FFmpeg 8.1.2 as a separate command-line program. FFmpeg is
licensed under the GNU Lesser General Public License version 2.1 or later for
this build. The full license text is in `COPYING.LGPLv2.1`.

## Upstream source

- Source: `https://ffmpeg.org/releases/ffmpeg-8.1.2.tar.xz`
- Signature: `https://ffmpeg.org/releases/ffmpeg-8.1.2.tar.xz.asc`
- Source size: `11,710,924` bytes
- Source SHA-256: `464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c`
- FFmpeg release-key fingerprint: `FCF986EA15E6E293A5644F10B4322F04D67658D8`
- PGP result: good signature made 2026-06-16 22:48:59 EDT

The source and public key were fetched directly from `ffmpeg.org` over HTTPS.
The source size, SHA-256, key fingerprint, and detached PGP signature were
verified before extraction.

## Apple Silicon artifact

- File: `ffmpeg-aarch64-apple-darwin`
- Size: `22,186,376` bytes
- SHA-256: `0c6c0dcac32f2b5a9f19e194fb449783f383a9b0051b068342dd38d85198e0a7`
- Architecture: arm64
- Minimum macOS: 11.0
- SDK: macOS 26.5
- Compiler reported by FFmpeg: Apple clang 21.0.0 (`clang-2100.1.1.101`)

The binary links only Apple system libraries/frameworks. It contains no
Homebrew paths or third-party dynamic libraries. FFmpeg reports network support
as disabled, and the reviewed binary has no socket, DNS, or Apple networking
imports.

## Packaged application artifact

Tauri preserves this reviewed executable byte-for-byte in the application
bundle. Meetily pins and verifies the source size and SHA-256 above before every
spawn. The sidecar keeps its entitlement-free linker signature and is sealed as
nested code by the outer application signature.

## Build configuration

The source was configured in a fresh temporary directory with this command.
`BUILD_ROOT` is the fresh build directory and `SDKROOT` is obtained from
`xcrun --sdk macosx --show-sdk-path` under the selected Xcode installation.

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
SDKROOT="$SDKROOT" \
MACOSX_DEPLOYMENT_TARGET=11.0 \
./configure \
  --prefix="$BUILD_ROOT/install" \
  --target-os=darwin \
  --arch=arm64 \
  --cc=/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang \
  --sysroot="$SDKROOT" \
  --extra-cflags=-mmacosx-version-min=11.0 \
  --extra-ldflags=-mmacosx-version-min=11.0 \
  --disable-shared \
  --enable-static \
  --disable-autodetect \
  --disable-network \
  --disable-gpl \
  --disable-nonfree \
  --disable-doc \
  --disable-debug \
  --disable-ffplay \
  --disable-ffprobe \
  --disable-avdevice \
  --disable-stripping

make -j8 ffmpeg
```

FFmpeg's optional GPL and nonfree components were explicitly disabled. Its
external-library autodetection was disabled, and FFmpeg libraries were linked
statically into the standalone executable. Apple system libraries remain
dynamically linked as required by macOS.
