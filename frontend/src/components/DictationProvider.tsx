'use client'

import { useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { register, unregister } from '@tauri-apps/plugin-global-shortcut'
import { toast } from 'sonner'

const DICTATION_SHORTCUT = 'Control+Alt+Space'

interface DictationResult {
  text: string
  copiedToClipboard: boolean
  pasted: boolean
}

export default function DictationProvider() {
  useEffect(() => {
    let mounted = true
    let active = false

    const registration = register(DICTATION_SHORTCUT, async (event) => {
      if (!mounted) return
      if (event.state === 'Pressed' && !active) {
        active = true
        try {
          await invoke('start_dictation')
          toast.loading('Listening… release Control + Option + Space to copy text', {
            id: 'dictation-status',
          })
        } catch (error) {
          active = false
          toast.error(String(error), { id: 'dictation-status' })
        }
      } else if (event.state === 'Released' && active) {
        active = false
        toast.loading('Transcribing locally…', { id: 'dictation-status' })
        try {
          const result = await invoke<DictationResult>('stop_dictation')
          if (result.pasted) {
            toast.success('Dictation inserted', { id: 'dictation-status' })
          } else if (result.copiedToClipboard) {
            toast.success('Copied—press ⌘V to paste', { id: 'dictation-status' })
          } else {
            toast.success('Dictation complete', { id: 'dictation-status' })
          }
        } catch (error) {
          toast.error(String(error), { id: 'dictation-status' })
        }
      }
    }).catch((error) => {
      toast.error(`Could not register dictation shortcut: ${String(error)}`)
    })

    return () => {
      mounted = false
      active = false
      invoke('cancel_dictation').catch(() => undefined)
      // Wait for an in-flight registration before unregistering so a fast
      // unmount cannot leave the global shortcut installed without a provider.
      registration
        .then(() => unregister(DICTATION_SHORTCUT))
        .catch(() => undefined)
    }
  }, [])

  return null
}
