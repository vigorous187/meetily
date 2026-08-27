import { describe, expect, test } from 'bun:test'
import {
  cleanupLegacyAutomaticMeetingStorage,
  LEGACY_AUTOMATIC_MEETING_STORAGE_KEYS,
} from '../../src/lib/automatic-meeting-lifecycle'

describe('cleanupLegacyAutomaticMeetingStorage', () => {
  test('removes stale frontend ownership and stop requests', () => {
    const removed: string[] = []

    cleanupLegacyAutomaticMeetingStorage({
      removeItem: (key) => removed.push(key),
    })

    expect(removed).toEqual([...LEGACY_AUTOMATIC_MEETING_STORAGE_KEYS])
    expect(removed).not.toContain('autoStartRecording')
  })

  test('is safe during server rendering', () => {
    expect(() => cleanupLegacyAutomaticMeetingStorage(undefined)).not.toThrow()
  })
})
