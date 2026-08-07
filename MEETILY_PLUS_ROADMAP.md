# Meetily Plus roadmap

Meetily Plus keeps Meetily's Tauri, Rust, local-audio, Whisper/Parakeet, and
local-summary architecture. It ports useful workflow ideas from OpenWhispr
without adding a second Electron runtime or requiring OpenWhispr Cloud.

Both upstream projects use the MIT license. New code in this fork is also MIT.

## Implemented

- Complete Markdown export:
  - Exports the editable AI summary and the entire transcript together.
  - Uses recording-relative timestamps.
  - Writes beside the local recording when a meeting folder exists.
  - Falls back to Downloads or Documents for folderless/imported meetings.
  - Never overwrites an earlier export; repeated exports receive a numeric suffix.

## Next: meeting auto-detection

Goal: offer a local notification when Zoom, Teams, Google Meet, or FaceTime
begins using the operating system's process and audio-session signals.

Implementation boundary:

- Add a Rust `MeetingDetector` trait.
- Implement macOS detection first, then Windows and Linux.
- Emit a Tauri event with the detected app and start time.
- Keep recording opt-in; detection must never silently record.

## Then: local speaker diarization

Goal: label speakers without sending audio to a cloud service.

Implementation boundary:

- Preserve Meetily's existing microphone/system-audio channel distinction.
- Add speaker embeddings only to the remote/system-audio channel.
- Run inference in a bounded worker so transcription remains responsive.
- Store stable speaker IDs separately from user-editable display names.
- Add a migration only after the diarization output contract is covered by tests.

## Then: local semantic meeting search

Goal: find meetings by meaning, not only exact transcript text.

Implementation boundary:

- Add local MiniLM embeddings behind a provider trait.
- Store chunk embeddings locally and update them in the background.
- Combine SQLite full-text search and vector similarity.
- Keep keyword-only search as a zero-download fallback.

## Later candidates

- Calendar context with explicit Google/Microsoft authorization.
- Voice fingerprints that users can name and delete.
- Public local API/MCP access guarded by a per-install token.
- Optional system-wide dictation as a separate feature, not part of the meeting
  recording lifecycle.

## Non-goals

- Depending on OpenWhispr Cloud.
- Copying its Electron shell into the Tauri application.
- Silent recording or consent bypass.
- Moving local-only Meetily features behind a paid gate.
