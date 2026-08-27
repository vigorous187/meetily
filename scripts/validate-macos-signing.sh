#!/bin/bash
set -euo pipefail

artifact="${1:-}"
if [[ -z "$artifact" || ! -e "$artifact" ]]; then
  echo "Usage: $0 /absolute/path/to/Meetily\ Plus.app-or-dmg" >&2
  exit 2
fi
case "$artifact" in
  /*) ;;
  *) echo "Signing validation requires an absolute artifact path" >&2; exit 2 ;;
esac

details="$(/usr/bin/codesign -dv --verbose=4 "$artifact" 2>&1)"
if [[ "$details" == *"Signature=adhoc"* ]]; then
  echo "Ad-hoc signatures are not valid for installed internal builds" >&2
  exit 3
fi
team_identifier="$(printf '%s\n' "$details" | /usr/bin/sed -n 's/^TeamIdentifier=//p' | /usr/bin/head -n 1)"
if [[ -z "$team_identifier" || "$team_identifier" == "not set" ]]; then
  echo "Installed internal builds require a stable Team Identifier" >&2
  exit 3
fi
if [[ "$details" != *"Authority=Apple Development:"* && "$details" != *"Authority=Developer ID Application:"* ]]; then
  echo "Expected an Apple Development or Developer ID Application signing authority" >&2
  exit 3
fi

/usr/bin/codesign --verify --deep --strict --verbose=4 "$artifact"
if [[ "$artifact" == *.app ]]; then
  entitlements="$(/usr/bin/codesign -d --entitlements - "$artifact" 2>/dev/null)"
  if [[ "$entitlements" != *'com.apple.security.device.audio-input'* ]]; then
    echo "Installed internal builds require the audio-input entitlement" >&2
    exit 3
  fi
  if [[ "$entitlements" != *'com.apple.security.automation.apple-events'* ]]; then
    echo "Installed internal builds require the Apple Events Automation entitlement" >&2
    exit 3
  fi
  entitlement_key_count="$(printf '%s\n' "$entitlements" | /usr/bin/grep -Ec '(<key>|\[Key\])')"
  if [[ "$entitlement_key_count" != "2" ]]; then
    echo "Installed internal builds contain unexpected entitlements" >&2
    exit 3
  fi
fi
echo "Verified stable Apple signature (TeamIdentifier=$team_identifier): $artifact"
