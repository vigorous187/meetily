'use client'

import { useEffect } from 'react'
import { toast } from 'sonner'
import { cleanupLegacyAutomaticMeetingStorage } from '@/lib/automatic-meeting-lifecycle'
import { autoCaptureService } from '@/services/autoCaptureService'

/**
 * Presents backend automatic-capture status globally.
 *
 * This component deliberately has no start/stop commands. Rust owns detection,
 * recording acknowledgement, retries, and ownership, so mounting or unmounting
 * React can never start or stop a recording.
 */
export default function MeetingDetectionProvider() {
  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | undefined

    cleanupLegacyAutomaticMeetingStorage()

    autoCaptureService.onStatusChanged((status) => {
      if (disposed) return

      if (status.state === 'recording') {
        toast.success('Automatic recording started', {
          id: 'auto-capture-status',
          description: status.message,
          duration: 6_000,
        })
      } else if (status.state === 'needsAction' || status.state === 'failed') {
        toast.error('Automatic capture needs attention', {
          id: 'auto-capture-status',
          description: status.message,
          duration: 10_000,
        })
      }
    }).then((stopListening) => {
      if (disposed) stopListening()
      else unlisten = stopListening
    }).catch((error) => {
      console.warn('Automatic capture status updates are unavailable:', error)
    })

    return () => {
      disposed = true
      unlisten?.()
    }
  }, [])

  return null
}
