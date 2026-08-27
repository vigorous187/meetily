import type { AutoCaptureHealth, AutoCaptureState, PermissionStatus } from '@/services/autoCaptureService'

export type HealthTone = 'healthy' | 'working' | 'warning' | 'error' | 'inactive'

export interface AutoCaptureHealthView {
  title: string
  tone: HealthTone
  summary: string
  detail: string
  retryLabel?: string
}

const STATE_TITLES: Record<AutoCaptureState, string> = {
  disabled: 'Off',
  observing: 'Ready',
  starting: 'Starting',
  retryScheduled: 'Retry scheduled',
  recording: 'Recording',
  stopping: 'Finishing',
  needsAction: 'Action required',
  failed: 'Needs attention',
}

export function permissionLabel(status: PermissionStatus): string {
  switch (status) {
    case 'granted': return 'Allowed'
    case 'denied': return 'Denied'
    case 'notDetermined': return 'Not requested'
    case 'unavailable': return 'Unavailable'
  }
}

export function getAutoCaptureHealthView(
  health: AutoCaptureHealth,
  now = new Date(),
): AutoCaptureHealthView {
  const title = STATE_TITLES[health.state]
  const fallbackDetail = health.enabled
    ? 'Meetily is monitoring local meeting signals.'
    : 'Turn on automatic capture to monitor meetings.'

  let tone: HealthTone = 'healthy'
  if (!health.enabled || health.state === 'disabled') tone = 'inactive'
  else if (health.state === 'starting' || health.state === 'stopping' || health.state === 'retryScheduled') tone = 'working'
  else if (health.state === 'needsAction' || health.degradedReasons.length > 0) tone = 'warning'
  else if (health.state === 'failed' || !health.detectorRunning) tone = 'error'

  let retryLabel: string | undefined
  if (health.nextRetryAtMs) {
    const seconds = Math.max(0, Math.ceil((health.nextRetryAtMs - now.getTime()) / 1000))
    retryLabel = seconds === 0 ? 'Retrying now' : `Retrying in ${seconds}s`
  }

  return {
    title,
    tone,
    summary: health.message || fallbackDetail,
    detail: health.errorCode ? `Code: ${health.errorCode}` : fallbackDetail,
    retryLabel,
  }
}
