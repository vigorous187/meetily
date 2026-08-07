export const MEETING_DETECTION_KEY = 'automatic_meeting_detection_enabled'
export const MEETING_DETECTION_CHANGED_EVENT = 'meetily-meeting-detection-changed'

export async function loadMeetingDetectionEnabled(): Promise<boolean> {
  const { Store } = await import('@tauri-apps/plugin-store')
  const store = await Store.load('preferences.json')
  return await store.get<boolean>(MEETING_DETECTION_KEY) ?? false
}

export async function saveMeetingDetectionEnabled(enabled: boolean): Promise<void> {
  const { Store } = await import('@tauri-apps/plugin-store')
  const store = await Store.load('preferences.json')
  await store.set(MEETING_DETECTION_KEY, enabled)
  await store.save()
  window.dispatchEvent(new CustomEvent(MEETING_DETECTION_CHANGED_EVENT, { detail: enabled }))
}
