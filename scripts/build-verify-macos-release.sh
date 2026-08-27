#!/bin/bash
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
repo_root="$(CDPATH= cd -- "$script_dir/.." && pwd)"
frontend_dir="$repo_root/frontend"
tauri_dir="$frontend_dir/src-tauri"
target_triple="aarch64-apple-darwin"
app_name="Meetily Plus.app"
rust_toolchain_dir="${MEETILY_RUST_TOOLCHAIN_DIR:-/Users/user/.local/share/meetily-rustup/toolchains/stable-aarch64-apple-darwin}"

: "${CARGO_HOME:?Set CARGO_HOME to the reviewed offline Cargo home}"
: "${MEETILY_ARCHIVE_PATH:?Set MEETILY_ARCHIVE_PATH to a new .zip output path}"
: "${MEETILY_DMG_PATH:?Set MEETILY_DMG_PATH to a new .dmg output path}"
signing_identity="${MEETILY_SIGNING_IDENTITY:-}"
dev_adhoc="${MEETILY_DEV_ADHOC:-0}"
if [[ -z "$signing_identity" ]]; then
  if [[ "$dev_adhoc" == "1" ]]; then
    signing_identity="-"
    echo "Building an explicit ad-hoc development artifact; it must remain outside /Applications" >&2
  else
    echo "Set MEETILY_SIGNING_IDENTITY to an Apple Development identity from Xcode" >&2
    exit 2
  fi
fi
if [[ "$signing_identity" != "-" && "$signing_identity" != "Apple Development:"* ]]; then
  echo "Internal builds require an Apple Development signing identity" >&2
  exit 2
fi

case "$CARGO_HOME" in
  /*) ;;
  *) echo "CARGO_HOME must be absolute" >&2; exit 2 ;;
esac
case "$MEETILY_ARCHIVE_PATH" in
  /*.zip) ;;
  *) echo "MEETILY_ARCHIVE_PATH must be an absolute .zip path" >&2; exit 2 ;;
esac
case "$MEETILY_ARCHIVE_PATH" in
  /Applications/*) echo "Release archives must not be written under /Applications" >&2; exit 2 ;;
esac
if [[ -e "$MEETILY_ARCHIVE_PATH" ]]; then
  echo "Refusing to overwrite existing archive: $MEETILY_ARCHIVE_PATH" >&2
  exit 2
fi
case "$MEETILY_DMG_PATH" in
  /*.dmg) ;;
  *) echo "MEETILY_DMG_PATH must be an absolute .dmg path" >&2; exit 2 ;;
esac
case "$MEETILY_DMG_PATH" in
  /Applications/*) echo "Release disk images must not be written under /Applications" >&2; exit 2 ;;
esac
if [[ -e "$MEETILY_DMG_PATH" ]]; then
  echo "Refusing to overwrite existing disk image: $MEETILY_DMG_PATH" >&2
  exit 2
fi

release_target_dir="${MEETILY_RELEASE_TARGET_DIR:-$repo_root/target}"
case "$release_target_dir" in
  /*) ;;
  *) echo "MEETILY_RELEASE_TARGET_DIR must be absolute" >&2; exit 2 ;;
esac
case "$release_target_dir" in
  /Applications|/Applications/*) echo "Release build output must not use /Applications" >&2; exit 2 ;;
esac

required_tools=(
  /usr/bin/codesign
  /usr/bin/security
  /usr/bin/ditto
  /usr/bin/file
  /usr/bin/hdiutil
  /usr/bin/lipo
  /usr/bin/otool
  /usr/bin/plutil
  /usr/bin/shasum
  /usr/bin/stat
  /usr/bin/vtool
  /usr/bin/xattr
)
for tool in "${required_tools[@]}"; do
  [[ -x "$tool" ]] || { echo "Required tool is unavailable: $tool" >&2; exit 2; }
done
if [[ "$signing_identity" != "-" ]] && ! /usr/bin/security find-identity -v -p codesigning | /usr/bin/grep -Fq "$signing_identity"; then
  echo "Requested Apple Development identity is not available in the keychain" >&2
  exit 2
fi
[[ -x /usr/libexec/PlistBuddy ]] || {
  echo "Required tool is unavailable: /usr/libexec/PlistBuddy" >&2
  exit 2
}
for frontend_tool in tauri next; do
  frontend_tool_path="$frontend_dir/node_modules/.bin/$frontend_tool"
  [[ -x "$frontend_tool_path" ]] || {
    echo "Reviewed frontend dependency is unavailable: $frontend_tool_path" >&2
    exit 2
  }
done

if [[ "$(/usr/bin/uname -s)" != "Darwin" || "$(/usr/bin/uname -m)" != "arm64" ]]; then
  echo "Release packaging is reviewed only for Apple Silicon macOS" >&2
  exit 2
fi
[[ -d "$CARGO_HOME" ]] || { echo "Reviewed CARGO_HOME does not exist" >&2; exit 2; }
[[ -x "$rust_toolchain_dir/bin/rustc" ]] || {
  echo "Reviewed rustc is unavailable: $rust_toolchain_dir/bin/rustc" >&2
  exit 2
}
[[ -x "$rust_toolchain_dir/lib/rustlib/$target_triple/bin/rust-objcopy" ]] || {
  echo "Reviewed rust-objcopy is unavailable for $target_triple" >&2
  exit 2
}

for removed_updater in \
  "$frontend_dir/src/components/UpdateCheckProvider.tsx" \
  "$frontend_dir/src/components/UpdateDialog.tsx" \
  "$frontend_dir/src/components/UpdateNotification.tsx" \
  "$frontend_dir/src/hooks/useUpdateCheck.ts" \
  "$frontend_dir/src/services/updateService.ts"; do
  [[ ! -e "$removed_updater" ]] || {
    echo "Updater source must remain removed: $removed_updater" >&2
    exit 3
  }
done

if [[ "$(/usr/libexec/PlistBuddy -c 'Print :com.apple.security.device.audio-input' "$tauri_dir/entitlements.plist")" != "true" ]]; then
  echo "The main executable must retain the audio-input entitlement" >&2
  exit 3
fi
if [[ "$(/usr/libexec/PlistBuddy -c 'Print :com.apple.security.automation.apple-events' "$tauri_dir/entitlements.plist")" != "true" ]]; then
  echo "The main executable must retain the Apple Events Automation entitlement" >&2
  exit 3
fi
entitlement_keys="$(/usr/bin/plutil -p "$tauri_dir/entitlements.plist" | /usr/bin/sed -n 's/^  "\([^"]*\)" =>.*/\1/p')"
if [[ "$(printf '%s\n' "$entitlement_keys" | /usr/bin/wc -l | /usr/bin/tr -d ' ')" != "2" ]] \
  || [[ "$entitlement_keys" != *"com.apple.security.device.audio-input"* ]] \
  || [[ "$entitlement_keys" != *"com.apple.security.automation.apple-events"* ]]; then
  echo "Unexpected main-executable entitlement: $entitlement_keys" >&2
  exit 3
fi

export CARGO_NET_OFFLINE=true
export CARGO_TARGET_DIR="$release_target_dir"
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
export MACOSX_DEPLOYMENT_TARGET=14.2
export XCODE_XCCONFIG_FILE="$repo_root/scripts/sandbox-xcode.xcconfig"
export PATH="$repo_root/scripts/sandbox-bin:$rust_toolchain_dir/bin:$rust_toolchain_dir/lib/rustlib/$target_triple/bin:$PATH"

(
  cd "$frontend_dir"
  ./node_modules/.bin/tauri build \
    --config '{"build":{"beforeBuildCommand":"./node_modules/.bin/next build"}}' \
    --target "$target_triple" \
    --bundles app
)

app_path="$release_target_dir/$target_triple/release/bundle/macos/$app_name"
[[ -d "$app_path" ]] || { echo "Built application was not found: $app_path" >&2; exit 4; }

sidecars=(ffmpeg llama-helper diarization-helper)
for sidecar in "${sidecars[@]}"; do
  sidecar_path="$app_path/Contents/MacOS/$sidecar"
  [[ -f "$sidecar_path" && ! -L "$sidecar_path" ]] || {
    echo "Packaged sidecar is missing or unsafe: $sidecar_path" >&2
    exit 4
  }
done

# Finder metadata and other extended attributes make an otherwise valid bundle
# fail strict verification after copying. Release archives contain none.
/usr/bin/xattr -cr "$app_path"

# Seal the reviewed ad-hoc sidecars as nested code and sign the outer app with
# the stable identity. Re-signing sidecars would invalidate their pinned hashes.
/usr/bin/codesign --force --sign "$signing_identity" --options runtime --timestamp=none \
  --entitlements "$tauri_dir/entitlements.plist" \
  "$app_path"

verify_file_record() {
  local path="$1"
  local expected_size="$2"
  local expected_sha256="$3"
  local actual_size actual_sha256
  actual_size="$(/usr/bin/stat -f '%z' "$path")"
  actual_sha256="$(/usr/bin/shasum -a 256 "$path" | /usr/bin/awk '{print $1}')"
  [[ "$actual_size" == "$expected_size" ]] || {
    echo "Size mismatch for $path: $actual_size != $expected_size" >&2
    return 1
  }
  [[ "$actual_sha256" == "$expected_sha256" ]] || {
    echo "SHA-256 mismatch for $path: $actual_sha256 != $expected_sha256" >&2
    return 1
  }
}

verify_macho() {
  local path="$1"
  [[ "$(/usr/bin/lipo -archs "$path")" == "arm64" ]] || {
    echo "Non-arm64 release executable: $path" >&2
    return 1
  }
  /usr/bin/vtool -show-build "$path" | /usr/bin/grep -Eq '^[[:space:]]+minos (11\.0|14\.2)$' || {
    echo "Unexpected deployment target: $path" >&2
    return 1
  }
  if /usr/bin/otool -L "$path" | /usr/bin/tail -n +2 | /usr/bin/awk '{print $1}' \
      | /usr/bin/grep -Ev '^(/usr/lib/|/System/Library/)' | /usr/bin/grep -q .; then
    echo "Non-system dynamic dependency in $path" >&2
    return 1
  fi
}

verify_signed_bundle() {
  local bundle="$1"
  /usr/bin/codesign --verify --deep --strict --verbose=4 "$bundle"
  if [[ "$signing_identity" != "-" ]]; then
    /bin/bash "$repo_root/scripts/validate-macos-signing.sh" "$bundle"
  fi

  local main_entitlements
  main_entitlements="$(/usr/bin/codesign -d --entitlements :- "$bundle" 2>/dev/null)"
  [[ "$main_entitlements" == *'<key>com.apple.security.device.audio-input</key>'* ]] || {
    echo "Main executable is missing audio-input entitlement" >&2
    return 1
  }
  [[ "$main_entitlements" == *'<key>com.apple.security.automation.apple-events</key>'* ]] || {
    echo "Main executable is missing Apple Events Automation entitlement" >&2
    return 1
  }
  [[ "$(printf '%s' "$main_entitlements" | /usr/bin/grep -c '<key>')" == "2" ]] || {
    echo "Main executable contains unexpected entitlements" >&2
    return 1
  }

  local sidecar sidecar_path sidecar_entitlements
  for sidecar in "${sidecars[@]}"; do
    sidecar_path="$bundle/Contents/MacOS/$sidecar"
    /usr/bin/codesign --verify --strict --verbose=4 "$sidecar_path"
    sidecar_entitlements="$(/usr/bin/codesign -d --entitlements :- "$sidecar_path" 2>/dev/null || true)"
    [[ "$sidecar_entitlements" != *'<key>'* ]] || {
      echo "Sidecar unexpectedly has entitlements: $sidecar" >&2
      return 1
    }
    verify_macho "$sidecar_path"
  done

  verify_file_record "$bundle/Contents/MacOS/ffmpeg" \
    22057536 9547d85ee85eb7d9480c517c9e224d739780e3f2c9e251e5fb585a1ffdcc5437
  verify_file_record "$bundle/Contents/MacOS/llama-helper" \
    5160736 eebae2a1e27acd0258a89630a889e470cdd6a8c896e73359f0d871595df3d296
  verify_file_record "$bundle/Contents/MacOS/diarization-helper" \
    23369056 03d245d0c69d60b6cae1f1b8e41d18bb7a1d1cda073d831735f882186a3f6773

  if /usr/bin/xattr -lr "$bundle" 2>/dev/null | /usr/bin/grep -q 'com.apple.FinderInfo'; then
    echo "FinderInfo remains in release bundle" >&2
    return 1
  fi
}

verify_signed_bundle "$app_path"

archive_parent="$(dirname -- "$MEETILY_ARCHIVE_PATH")"
[[ -d "$archive_parent" ]] || { echo "Archive parent does not exist: $archive_parent" >&2; exit 5; }
disk_image_parent="$(dirname -- "$MEETILY_DMG_PATH")"
[[ -d "$disk_image_parent" ]] || { echo "Disk-image parent does not exist: $disk_image_parent" >&2; exit 5; }
/usr/bin/ditto -c -k --keepParent "$app_path" "$MEETILY_ARCHIVE_PATH"

verify_root="$(/usr/bin/mktemp -d "${TMPDIR:-/tmp}/meetily-release-verify.XXXXXX")"
dmg_stage="$(/usr/bin/mktemp -d "${TMPDIR:-/tmp}/meetily-dmg-stage.XXXXXX")"
dmg_mount="$(/usr/bin/mktemp -d "${TMPDIR:-/tmp}/meetily-dmg-mount.XXXXXX")"
dmg_attached=false
cleanup() {
  if [[ "$dmg_attached" == true ]]; then
    /usr/bin/hdiutil detach "$dmg_mount" -quiet || true
  fi
  /bin/rm -rf -- "$verify_root" "$dmg_stage" "$dmg_mount"
}
trap cleanup EXIT
/usr/bin/ditto -x -k "$MEETILY_ARCHIVE_PATH" "$verify_root"
extracted_app="$verify_root/$app_name"
[[ -d "$extracted_app" ]] || { echo "Archive did not contain exactly $app_name" >&2; exit 5; }
[[ "$(find "$verify_root" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')" == "1" ]] || {
  echo "Archive contains unexpected top-level entries" >&2
  exit 5
}

verify_signed_bundle "$extracted_app"
/usr/bin/diff -qr "$app_path" "$extracted_app"

/usr/bin/ditto "$app_path" "$dmg_stage/$app_name"
/bin/ln -s /Applications "$dmg_stage/Applications"
/usr/bin/hdiutil create -quiet \
  -volname "Meetily Plus 0.4.4" \
  -srcfolder "$dmg_stage" \
  -format UDZO \
  "$MEETILY_DMG_PATH"
/usr/bin/codesign --force --sign "$signing_identity" --timestamp=none "$MEETILY_DMG_PATH"
/usr/bin/codesign --verify --strict --verbose=4 "$MEETILY_DMG_PATH"
if [[ "$signing_identity" != "-" ]]; then
  /bin/bash "$repo_root/scripts/validate-macos-signing.sh" "$MEETILY_DMG_PATH"
fi

/usr/bin/hdiutil attach -quiet -readonly -nobrowse -mountpoint "$dmg_mount" "$MEETILY_DMG_PATH"
dmg_attached=true
mounted_app="$dmg_mount/$app_name"
[[ -d "$mounted_app" ]] || { echo "Disk image does not contain $app_name" >&2; exit 5; }
[[ -L "$dmg_mount/Applications" && "$(/usr/bin/readlink "$dmg_mount/Applications")" == "/Applications" ]] || {
  echo "Disk image is missing the Applications install link" >&2
  exit 5
}
verify_signed_bundle "$mounted_app"
/usr/bin/diff -qr "$app_path" "$mounted_app"
/usr/bin/hdiutil detach "$dmg_mount" -quiet
dmg_attached=false

echo "Verified release archive: $MEETILY_ARCHIVE_PATH"
echo "Verified signed installer: $MEETILY_DMG_PATH"
