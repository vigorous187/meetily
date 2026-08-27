import { describe, expect, test } from 'bun:test'
import {
  AUTO_CAPTURE_STATUS_EVENT,
  createAutoCaptureClient,
  type AutoCaptureStatusChanged,
} from '../../src/services/autoCaptureService'

const backendHealth = {
  enabled: true,
  detectorRunning: true,
  state: 'observing' as const,
  attempt: 0,
  degradedReasons: [],
  message: 'Ready',
}

describe('auto capture client', () => {
  test('maps every health action to its typed Tauri command', async () => {
    const calls: Array<{ command: string, args?: Record<string, unknown> }> = []
    const invoke = async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
      calls.push({ command, args })
      if (command === 'get_auto_capture_permissions') return [] as T
      if (command === 'get_launch_at_login_status' || command === 'set_launch_at_login') {
        return { enabled: true, available: true, message: 'Enabled' } as T
      }
      if (command === 'request_auto_capture_permission') {
        return { kind: 'screenRecording', status: 'granted', message: 'Allowed' } as T
      }
      if (command === 'export_auto_capture_diagnostics') return '/tmp/diagnostics.zip' as T
      return backendHealth as T
    }
    const listen = async () => () => undefined
    const client = createAutoCaptureClient(invoke, listen)

    await client.getHealth()
    await client.setEnabled(false)
    await client.setLaunchAtLogin(true)
    await client.requestPermission('screenRecording')
    await client.exportDiagnostics()

    expect(calls).toContainEqual({ command: 'set_auto_capture_enabled', args: { enabled: false } })
    expect(calls).toContainEqual({ command: 'set_launch_at_login', args: { enabled: true } })
    expect(calls).toContainEqual({ command: 'request_auto_capture_permission', args: { kind: 'screenRecording' } })
    expect(calls.at(-1)?.command).toBe('export_auto_capture_diagnostics')
  })

  test('mount-like subscriptions never invoke start or stop commands', async () => {
    const commands: string[] = []
    const events: string[] = []
    let handler: ((event: { payload: AutoCaptureStatusChanged }) => void) | undefined
    const invoke = async <T>(command: string): Promise<T> => {
      commands.push(command)
      return backendHealth as T
    }
    const listen = async <T>(event: string, next: (event: { payload: T }) => void) => {
      events.push(event)
      handler = next as (event: { payload: AutoCaptureStatusChanged }) => void
      return () => undefined
    }
    const client = createAutoCaptureClient(invoke, listen)

    const unlistenFirst = await client.onStatusChanged(() => undefined)
    unlistenFirst()
    const unlistenAfterRemount = await client.onStatusChanged(() => undefined)
    handler?.({ payload: {
      state: 'recording',
      attempt: 1,
      degradedReasons: [],
      message: 'Recording started',
    } })
    unlistenAfterRemount()

    expect(events).toEqual([AUTO_CAPTURE_STATUS_EVENT, AUTO_CAPTURE_STATUS_EVENT])
    expect(commands).toEqual([])
    expect(commands).not.toContain('start_recording')
    expect(commands).not.toContain('stop_recording')
  })
})
