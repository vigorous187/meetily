import { describe, expect, test } from 'bun:test'
import { getAutoCaptureHealthView, permissionLabel } from '../../src/lib/auto-capture-health'
import type { AutoCaptureHealth } from '../../src/services/autoCaptureService'

function health(overrides: Partial<AutoCaptureHealth> = {}): AutoCaptureHealth {
  return {
    enabled: true,
    detectorRunning: true,
    state: 'observing',
    attempt: 0,
    degradedReasons: [],
    message: 'Monitoring for meetings',
    permissions: [],
    launchAtLogin: { enabled: true, available: true, message: 'Enabled' },
    ...overrides,
  }
}

describe('Automatic Capture Health presentation', () => {
  test('renders retry timing and action-required state', () => {
    const now = new Date('2026-08-27T12:00:00.000Z')
    const view = getAutoCaptureHealthView(health({
      state: 'retryScheduled',
      attempt: 2,
      nextRetryAtMs: now.getTime() + 5_000,
      message: 'Recorder is temporarily busy',
    }), now)

    expect(view.title).toBe('Retry scheduled')
    expect(view.tone).toBe('working')
    expect(view.retryLabel).toBe('Retrying in 5s')
  })

  test('makes degraded and detector failures visible', () => {
    expect(getAutoCaptureHealthView(health({ degradedReasons: ['system_audio_unavailable'] })).tone).toBe('warning')
    expect(getAutoCaptureHealthView(health({ state: 'failed', detectorRunning: false })).tone).toBe('error')
    expect(getAutoCaptureHealthView(health({ enabled: false, state: 'disabled' })).tone).toBe('inactive')
  })

  test('uses user-safe permission labels', () => {
    expect(permissionLabel('granted')).toBe('Allowed')
    expect(permissionLabel('notDetermined')).toBe('Not requested')
    expect(permissionLabel('denied')).toBe('Denied')
  })
})
