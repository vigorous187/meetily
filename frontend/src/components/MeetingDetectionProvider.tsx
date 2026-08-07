'use client'

import { useEffect } from 'react'
import { useRouter } from 'next/navigation'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { toast } from 'sonner'
import {
  loadMeetingDetectionEnabled,
  MEETING_DETECTION_CHANGED_EVENT,
  saveMeetingDetectionEnabled,
} from '@/lib/meeting-detection-preference'
import {
  AUTOMATIC_MEETING_SESSION_KEY,
  AUTOMATIC_MEETING_STOP_REQUEST_KEY,
} from '@/lib/automatic-meeting-lifecycle'

interface MeetingDetectedPayload {
  application: string
}

export default function MeetingDetectionProvider() {
  const router = useRouter()

  useEffect(() => {
    let disposed = false
    let unlistenDetected: (() => void) | undefined
    let unlistenEnded: (() => void) | undefined
    let unlistenError: (() => void) | undefined
    let enabled = false

    const setEnabled = async (nextEnabled: boolean) => {
      enabled = nextEnabled
      if (nextEnabled) {
        await invoke('start_meeting_detection')
      } else {
        toast.dismiss('meeting-detected')
        await invoke('stop_meeting_detection')
      }
    }

    const handlePreferenceChange = (event: Event) => {
      const nextEnabled = Boolean((event as CustomEvent<boolean>).detail)
      setEnabled(nextEnabled).catch(async () => {
        if (nextEnabled) {
          await saveMeetingDetectionEnabled(false).catch(() => undefined)
          toast.error('Automatic meeting detection is unavailable', {
            description: 'Restart Meetily, then enable it again in Recording Settings.',
          })
        }
      })
    }

    const setup = async () => {
      unlistenDetected = await listen<MeetingDetectedPayload>('meeting-detected', async ({ payload }) => {
        if (!enabled) return

        // Never take ownership of a recording the user started manually.
        // Otherwise the matching meeting-ended event could stop that manual
        // recording unexpectedly.
        const recordingAlreadyActive = await invoke<boolean>('is_recording').catch(() => false)
        if (recordingAlreadyActive) return

        sessionStorage.setItem(AUTOMATIC_MEETING_SESSION_KEY, payload.application)
        toast.success(`${payload.application} meeting detected`, {
          id: 'meeting-detected',
          description: 'Recording is starting automatically.',
          duration: 6_000,
        })

        if (window.location.pathname === '/') {
          window.dispatchEvent(new CustomEvent('start-recording-from-sidebar'))
        } else {
          sessionStorage.setItem('autoStartRecording', 'true')
          router.push('/')
        }
      })
      if (disposed) { unlistenDetected(); return }

      unlistenEnded = await listen<MeetingDetectedPayload>('meeting-ended', ({ payload }) => {
        if (!enabled) return

        const automaticApplication = sessionStorage.getItem(AUTOMATIC_MEETING_SESSION_KEY)
        if (!automaticApplication) return

        sessionStorage.removeItem(AUTOMATIC_MEETING_SESSION_KEY)
        sessionStorage.setItem(AUTOMATIC_MEETING_STOP_REQUEST_KEY, 'true')
        if (window.location.pathname === '/') {
          window.dispatchEvent(new CustomEvent('stop-recording-automatically'))
        } else {
          router.push('/')
        }
        toast.info(`${payload.application} meeting ended`, {
          id: 'meeting-ended',
          description: 'Recording is stopping and being saved automatically.',
          duration: 8_000,
        })
      })
      if (disposed) { unlistenEnded(); return }

      unlistenError = await listen<string>('meeting-detection-error', async () => {
        console.warn('Automatic meeting detection unavailable')
        enabled = false
        await saveMeetingDetectionEnabled(false).catch(() => undefined)
        toast.error('Automatic meeting detection stopped', {
          description: 'Restart Meetily, then enable it again in Recording Settings.',
        })
      })
      if (disposed) { unlistenError(); return }

      window.addEventListener(MEETING_DETECTION_CHANGED_EVENT, handlePreferenceChange)
      await setEnabled(await loadMeetingDetectionEnabled())
    }

    setup().catch(async (error) => {
      console.warn('Automatic meeting detection could not start:', error)
      if (!disposed && enabled) {
        enabled = false
        await saveMeetingDetectionEnabled(false).catch(() => undefined)
        toast.error('Automatic meeting detection is unavailable', {
          description: 'Restart Meetily, then enable it again in Recording Settings.',
        })
      }
    })

    return () => {
      disposed = true
      window.removeEventListener(MEETING_DETECTION_CHANGED_EVENT, handlePreferenceChange)
      unlistenDetected?.()
      unlistenEnded?.()
      unlistenError?.()
      invoke('stop_meeting_detection').catch(() => undefined)
    }
  }, [router])

  return null
}
