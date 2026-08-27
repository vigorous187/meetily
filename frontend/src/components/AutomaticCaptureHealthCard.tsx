'use client'

import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  Download,
  RefreshCw,
  ShieldAlert,
} from 'lucide-react'
import { toast } from 'sonner'
import { Switch } from '@/components/ui/switch'
import { getAutoCaptureHealthView, permissionLabel } from '@/lib/auto-capture-health'
import {
  autoCaptureService,
  type AutoCaptureHealth,
  type PermissionKind,
} from '@/services/autoCaptureService'

const TONE_STYLES = {
  healthy: 'border-emerald-200 bg-emerald-50 text-emerald-800',
  working: 'border-blue-200 bg-blue-50 text-blue-800',
  warning: 'border-amber-200 bg-amber-50 text-amber-900',
  error: 'border-red-200 bg-red-50 text-red-800',
  inactive: 'border-gray-200 bg-gray-50 text-gray-700',
} as const

function displayPermissionKind(kind: PermissionKind): string {
  if (kind === 'screenRecording') return 'Screen Recording'
  if (kind === 'browserAutomation') return 'Browser Automation'
  return 'Audio Capture'
}

export function AutomaticCaptureHealthCard() {
  const [health, setHealth] = useState<AutoCaptureHealth | null>(null)
  const [loading, setLoading] = useState(true)
  const [busyAction, setBusyAction] = useState<string | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      const nextHealth = await autoCaptureService.getHealth()
      setHealth(nextHealth)
      setLoadError(null)
    } catch (error) {
      setLoadError(error instanceof Error ? error.message : String(error))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | undefined

    refresh().catch(() => undefined)
    autoCaptureService.onStatusChanged((status) => {
      if (disposed) return
      setHealth((current) => current ? { ...current, ...status } : current)
    }).then((stopListening) => {
      if (disposed) stopListening()
      else unlisten = stopListening
    }).catch(() => {
      // The health request above remains the visible source of truth if event
      // subscription is temporarily unavailable.
    })

    return () => {
      disposed = true
      unlisten?.()
    }
  }, [refresh])

  const view = useMemo(() => health ? getAutoCaptureHealthView(health) : null, [health])

  const runAction = async (name: string, action: () => Promise<void>) => {
    setBusyAction(name)
    try {
      await action()
    } catch (error) {
      toast.error('Automatic capture could not be updated', {
        description: error instanceof Error ? error.message : String(error),
      })
    } finally {
      setBusyAction(null)
    }
  }

  const setEnabled = (enabled: boolean) => runAction('enabled', async () => {
    const nextHealth = await autoCaptureService.setEnabled(enabled)
    setHealth(nextHealth)
    toast.success(enabled ? 'Automatic capture enabled' : 'Automatic capture disabled')
  })

  const setLaunchAtLogin = (enabled: boolean) => runAction('launch', async () => {
    const launchAtLogin = await autoCaptureService.setLaunchAtLogin(enabled)
    setHealth((current) => current ? { ...current, launchAtLogin } : current)
    if (!enabled && health?.enabled) {
      toast.warning('Launch at login is off', {
        description: 'Meetily must be running to capture meetings automatically.',
      })
    }
  })

  const requestPermission = (kind: PermissionKind) => runAction(`permission-${kind}`, async () => {
    await autoCaptureService.requestPermission(kind)
    await refresh()
  })

  const exportDiagnostics = () => runAction('export', async () => {
    const path = await autoCaptureService.exportDiagnostics()
    toast.success('Diagnostics exported', { description: path })
  })

  if (loading) {
    return (
      <div className="rounded-lg border border-gray-200 p-4" aria-busy="true">
        <div className="h-5 w-56 animate-pulse rounded bg-gray-200" />
        <div className="mt-3 h-16 animate-pulse rounded bg-gray-100" />
      </div>
    )
  }

  if (!health || !view) {
    return (
      <div className="rounded-lg border border-red-200 bg-red-50 p-4 text-red-900">
        <div className="flex items-start gap-3">
          <ShieldAlert className="mt-0.5 h-5 w-5 shrink-0" />
          <div className="flex-1">
            <div className="font-medium">Automatic Capture Health unavailable</div>
            <div className="mt-1 text-sm">{loadError || 'Meetily could not read detector status.'}</div>
          </div>
          <button className="rounded-md border border-red-300 px-3 py-1.5 text-sm" onClick={() => void refresh()}>
            Retry
          </button>
        </div>
      </div>
    )
  }

  const StatusIcon = view.tone === 'healthy'
    ? CheckCircle2
    : view.tone === 'warning' || view.tone === 'error'
      ? AlertTriangle
      : Activity

  return (
    <section className="rounded-lg border border-gray-200 bg-white p-5" aria-labelledby="auto-capture-health-heading">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h4 id="auto-capture-health-heading" className="text-base font-semibold text-gray-900">
            Automatic Capture Health
          </h4>
          <p className="mt-1 text-sm text-gray-600">
            Backend-owned meeting detection continues when this window is hidden or reloaded.
          </p>
        </div>
        <Switch
          aria-label="Enable automatic capture"
          checked={health.enabled}
          disabled={busyAction === 'enabled'}
          onCheckedChange={(enabled) => void setEnabled(enabled)}
        />
      </div>

      <div className={`mt-4 rounded-md border p-3 ${TONE_STYLES[view.tone]}`} role="status">
        <div className="flex items-start gap-2">
          <StatusIcon className="mt-0.5 h-4 w-4 shrink-0" />
          <div>
            <div className="font-medium">{view.title}</div>
            <div className="text-sm">{view.summary}</div>
            {view.retryLabel && <div className="mt-1 text-xs font-medium">{view.retryLabel}</div>}
            {health.errorCode && <div className="mt-1 text-xs">Error code: {health.errorCode}</div>}
          </div>
        </div>
      </div>

      {health.degradedReasons.length > 0 && (
        <div className="mt-3 rounded-md border border-amber-200 bg-amber-50 p-3 text-sm text-amber-900">
          <div className="font-medium">Recording is degraded</div>
          <ul className="mt-1 list-disc pl-5">
            {health.degradedReasons.map((reason) => <li key={reason}>{reason}</li>)}
          </ul>
        </div>
      )}

      <div className="mt-4 grid gap-3 sm:grid-cols-2">
        {health.permissions.map((permission) => (
          <div key={permission.kind} className="rounded-md border border-gray-200 p-3">
            <div className="flex items-center justify-between gap-3">
              <div>
                <div className="text-sm font-medium text-gray-900">{displayPermissionKind(permission.kind)}</div>
                <div className="text-xs text-gray-600">{permissionLabel(permission.status)}</div>
              </div>
              {permission.status !== 'granted' && permission.status !== 'unavailable' && (
                <button
                  className="rounded-md border border-gray-300 px-2.5 py-1.5 text-xs hover:bg-gray-50 disabled:opacity-50"
                  disabled={busyAction === `permission-${permission.kind}`}
                  onClick={() => void requestPermission(permission.kind)}
                >
                  {permission.status === 'notDetermined' ? 'Allow' : 'Open settings'}
                </button>
              )}
            </div>
            {permission.message && <p className="mt-2 text-xs text-gray-600">{permission.message}</p>}
          </div>
        ))}
      </div>

      <div className="mt-4 flex items-center justify-between gap-4 rounded-md border border-gray-200 p-3">
        <div>
          <div className="text-sm font-medium text-gray-900">Launch Meetily at login</div>
          <div className="text-xs text-gray-600">
            Required for automatic capture after a restart. The app opens hidden.
          </div>
          {!health.launchAtLogin.available && (
            <div className="mt-1 text-xs text-red-700">{health.launchAtLogin.message || 'Unavailable'}</div>
          )}
        </div>
        <Switch
          aria-label="Launch Meetily at login"
          checked={health.launchAtLogin.enabled}
          disabled={!health.launchAtLogin.available || busyAction === 'launch'}
          onCheckedChange={(enabled) => void setLaunchAtLogin(enabled)}
        />
      </div>

      {health.lastResult && (
        <div className="mt-4 text-sm text-gray-600">
          <span className="font-medium text-gray-800">Last result:</span>{' '}
          {health.lastResult}
        </div>
      )}

      <div className="mt-4 flex flex-wrap gap-2">
        <button
          className="flex items-center gap-2 rounded-md border border-gray-300 px-3 py-2 text-sm hover:bg-gray-50 disabled:opacity-50"
          disabled={busyAction !== null}
          onClick={() => void refresh()}
        >
          <RefreshCw className="h-4 w-4" /> Refresh
        </button>
        <button
          className="flex items-center gap-2 rounded-md border border-gray-300 px-3 py-2 text-sm hover:bg-gray-50 disabled:opacity-50"
          disabled={busyAction !== null}
          onClick={() => void exportDiagnostics()}
        >
          <Download className="h-4 w-4" /> Export diagnostics
        </button>
      </div>
    </section>
  )
}
