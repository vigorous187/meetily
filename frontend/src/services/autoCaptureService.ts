import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

export const AUTO_CAPTURE_STATUS_EVENT = 'auto-capture-status-changed'

export type AutoCaptureState =
  | 'disabled'
  | 'observing'
  | 'starting'
  | 'retryScheduled'
  | 'recording'
  | 'stopping'
  | 'needsAction'
  | 'failed'

export type PermissionKind = 'screenRecording' | 'browserAutomation' | 'audioCapture'
export type PermissionStatus = 'granted' | 'denied' | 'notDetermined' | 'unavailable'

export interface PermissionState {
  kind: PermissionKind
  status: PermissionStatus
  errorCode?: string | null
  message: string
}

export interface LaunchAtLoginStatus {
  enabled: boolean
  available: boolean
  errorCode?: string | null
  message: string
}

interface BackendAutoCaptureHealth {
  enabled: boolean
  detectorRunning: boolean
  state: AutoCaptureState
  sessionId?: string | null
  candidate?: string | null
  attempt: number
  recordingId?: string | null
  nextRetryAtMs?: number | null
  degradedReasons: string[]
  errorCode?: string | null
  message: string
  lastResult?: string | null
  permissions?: PermissionState[]
  launchAtLogin?: LaunchAtLoginStatus
}

export interface AutoCaptureHealth extends BackendAutoCaptureHealth {
  permissions: PermissionState[]
  launchAtLogin: LaunchAtLoginStatus
}

export interface AutoCaptureStatusChanged {
  sessionId?: string | null
  candidate?: string | null
  state: AutoCaptureState
  attempt: number
  recordingId?: string | null
  nextRetryAtMs?: number | null
  degradedReasons: string[]
  errorCode?: string | null
  message: string
}

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>
type Listen = <T>(
  event: string,
  handler: (event: { payload: T }) => void,
) => Promise<UnlistenFn>

export interface AutoCaptureClient {
  getHealth(): Promise<AutoCaptureHealth>
  setEnabled(enabled: boolean): Promise<AutoCaptureHealth>
  setLaunchAtLogin(enabled: boolean): Promise<LaunchAtLoginStatus>
  requestPermission(kind: PermissionKind): Promise<PermissionState>
  exportDiagnostics(): Promise<string>
  onStatusChanged(callback: (status: AutoCaptureStatusChanged) => void): Promise<UnlistenFn>
}

const unavailableLaunchStatus: LaunchAtLoginStatus = {
  enabled: false,
  available: false,
  errorCode: 'launch_status_unavailable',
  message: 'Launch at login status is unavailable.',
}

export function createAutoCaptureClient(
  invokeCommand: Invoke = invoke,
  listenForEvent: Listen = listen,
): AutoCaptureClient {
  const enrichHealth = async (health: BackendAutoCaptureHealth): Promise<AutoCaptureHealth> => {
    const [permissions, launchAtLogin] = await Promise.all([
      health.permissions
        ? Promise.resolve(health.permissions)
        : invokeCommand<PermissionState[]>('get_auto_capture_permissions').catch(() => []),
      health.launchAtLogin
        ? Promise.resolve(health.launchAtLogin)
        : invokeCommand<LaunchAtLoginStatus>('get_launch_at_login_status').catch(() => unavailableLaunchStatus),
    ])

    return { ...health, permissions, launchAtLogin }
  }

  return {
    getHealth: async () => enrichHealth(
      await invokeCommand<BackendAutoCaptureHealth>('get_auto_capture_health'),
    ),
    setEnabled: async (enabled) => enrichHealth(
      await invokeCommand<BackendAutoCaptureHealth>('set_auto_capture_enabled', { enabled }),
    ),
    setLaunchAtLogin: (enabled) =>
      invokeCommand<LaunchAtLoginStatus>('set_launch_at_login', { enabled }),
    requestPermission: (kind) =>
      invokeCommand<PermissionState>('request_auto_capture_permission', { kind }),
    exportDiagnostics: () =>
      invokeCommand<string>('export_auto_capture_diagnostics'),
    onStatusChanged: (callback) =>
      listenForEvent<AutoCaptureStatusChanged>(AUTO_CAPTURE_STATUS_EVENT, ({ payload }) => callback(payload)),
  }
}

export const autoCaptureService = createAutoCaptureClient()
