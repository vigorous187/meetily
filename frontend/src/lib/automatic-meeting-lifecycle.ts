/**
 * Storage flags used by the retired frontend-owned automatic-capture flow.
 *
 * Automatic capture now belongs entirely to Rust. These keys are only kept so
 * upgraded installs can remove stale ownership/stop requests left by v0.4.3.
 */
export const LEGACY_AUTOMATIC_MEETING_STORAGE_KEYS = [
  'automaticMeetingRecording',
  'automaticMeetingStopRequested',
] as const

export interface SessionStorageLike {
  removeItem(key: string): void
}

export function cleanupLegacyAutomaticMeetingStorage(
  storage: SessionStorageLike | undefined = typeof window === 'undefined' ? undefined : window.sessionStorage,
): void {
  if (!storage) return

  for (const key of LEGACY_AUTOMATIC_MEETING_STORAGE_KEYS) {
    storage.removeItem(key)
  }
}
