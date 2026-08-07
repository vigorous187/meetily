'use client';

import React, { createContext, useContext, useState, useEffect } from 'react';
import { usePathname, useRouter } from 'next/navigation';
import Analytics from '@/lib/analytics';
import { invoke } from '@tauri-apps/api/core';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import { getSummaryPollDecision } from '@/lib/summary-polling';


interface SidebarItem {
  id: string;
  title: string;
  type: 'folder' | 'file';
  children?: SidebarItem[];
}

export interface CurrentMeeting {
  id: string;
  title: string;
}

// Search result type for transcript search
export interface TranscriptSearchResult {
  id: string;
  title: string;
  matchContext: string;
  timestamp: string;
}

interface SidebarContextType {
  currentMeeting: CurrentMeeting | null;
  setCurrentMeeting: (meeting: CurrentMeeting | null) => void;
  sidebarItems: SidebarItem[];
  isCollapsed: boolean;
  toggleCollapse: () => void;
  meetings: CurrentMeeting[];
  setMeetings: (meetings: CurrentMeeting[]) => void;
  isMeetingActive: boolean;
  setIsMeetingActive: (active: boolean) => void;
  handleRecordingToggle: () => void;
  searchTranscripts: (query: string) => Promise<void>;
  searchResults: TranscriptSearchResult[];
  isSearching: boolean;
  searchError: string | null;
  setServerAddress: (address: string) => void;
  serverAddress: string;
  transcriptServerAddress: string;
  setTranscriptServerAddress: (address: string) => void;
  // Summary polling management
  activeSummaryPolls: Map<string, ReturnType<typeof setTimeout>>;
  startSummaryPolling: (
    meetingId: string,
    processId: string,
    onUpdate: (result: any, isCurrentPoll: () => boolean) => void | Promise<void>
  ) => void;
  stopSummaryPolling: (meetingId: string) => void;
  // Refetch meetings from backend
  refetchMeetings: () => Promise<void>;

}

const SidebarContext = createContext<SidebarContextType | null>(null);

export const useSidebar = () => {
  const context = useContext(SidebarContext);
  if (!context) {
    throw new Error('useSidebar must be used within a SidebarProvider');
  }
  return context;
};

export function SidebarProvider({ children }: { children: React.ReactNode }) {
  const [currentMeeting, setCurrentMeeting] = useState<CurrentMeeting | null>({ id: 'intro-call', title: '+ New Call' });
  const [isCollapsed, setIsCollapsed] = useState(true);
  const [meetings, setMeetings] = useState<CurrentMeeting[]>([]);
  const [sidebarItems, setSidebarItems] = useState<SidebarItem[]>([]);
  const [isMeetingActive, setIsMeetingActive] = useState(false);
  const [searchResults, setSearchResults] = useState<TranscriptSearchResult[]>([]);
  const [isSearching, setIsSearching] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [serverAddress, setServerAddress] = useState('');
  const [transcriptServerAddress, setTranscriptServerAddress] = useState('');
  const [activeSummaryPolls, setActiveSummaryPolls] = useState<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  const activeSummaryPollsRef = React.useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  const summaryPollGenerationsRef = React.useRef<Map<string, number>>(new Map());
  const isSummaryPollingMountedRef = React.useRef(true);
  const searchRequestId = React.useRef(0);

  // Use recording state from RecordingStateContext (single source of truth)
  const { isRecording } = useRecordingState();

  const pathname = usePathname();
  const router = useRouter();

  // Extract fetchMeetings as a reusable function
  const fetchMeetings = React.useCallback(async () => {
    if (serverAddress) {
      try {
        const meetings = await invoke('api_get_meetings') as Array<{ id: string, title: string }>;
        const transformedMeetings = meetings.map((meeting: any) => ({
          id: meeting.id,
          title: meeting.title
        }));
        setMeetings(transformedMeetings);
        Analytics.trackBackendConnection(true);
      } catch (error) {
        console.error('Error fetching meetings:', error);
        setMeetings([]);
        Analytics.trackBackendConnection(false, error instanceof Error ? error.message : 'Unknown error');
      }
    }
  }, [serverAddress]);

  useEffect(() => {
    fetchMeetings();
  }, [serverAddress, fetchMeetings]);

  useEffect(() => {
    const fetchSettings = async () => {
      setServerAddress('http://localhost:5167');
      setTranscriptServerAddress('http://127.0.0.1:8178/stream');
    };
    fetchSettings();
  }, []);

  const baseItems: SidebarItem[] = [
    {
      id: 'meetings',
      title: 'Meeting Notes',
      type: 'folder' as const,
      children: [
        ...meetings.map(meeting => ({ id: meeting.id, title: meeting.title, type: 'file' as const }))
      ]
    },
  ];


  const toggleCollapse = () => {
    setIsCollapsed(!isCollapsed);
  };

  // Update current meeting when on home page
  useEffect(() => {
    if (pathname === '/') {
      setCurrentMeeting({ id: 'intro-call', title: '+ New Call' });
    }
    setSidebarItems(baseItems);
  }, [pathname]);

  // Update sidebar items when meetings change
  useEffect(() => {
    setSidebarItems(baseItems);
  }, [meetings]);

  // Function to handle recording toggle from sidebar
  const handleRecordingToggle = () => {
    if (!isRecording) {
      // Check if already on home page
      if (pathname === '/') {
        // Already on home - trigger recording directly via custom event
        console.log('Triggering recording from sidebar (already on home page)');
        window.dispatchEvent(new CustomEvent('start-recording-from-sidebar'));
      } else {
        // Not on home - navigate and use auto-start mechanism
        console.log('Navigating to home page with auto-start flag');
        sessionStorage.setItem('autoStartRecording', 'true');
        router.push('/');
      }

      // Track recording initiation from sidebar
      Analytics.trackButtonClick('start_recording', 'sidebar');
    }
    // The actual recording start/stop is handled in the Home component
  };

  // Function to search through meeting transcripts
  const searchTranscripts = React.useCallback(async (query: string) => {
    const requestId = ++searchRequestId.current;
    if (!query.trim()) {
      setSearchResults([]);
      setIsSearching(false);
      setSearchError(null);
      return;
    }

    try {
      setIsSearching(true);
      setSearchError(null);
      const results = await invoke('api_search_transcripts', { query }) as TranscriptSearchResult[];
      if (requestId === searchRequestId.current) {
        setSearchResults(results);
      }
    } catch (error) {
      console.error('Error searching transcripts:', error);
      if (requestId === searchRequestId.current) {
        setSearchResults([]);
        setSearchError('Search is temporarily unavailable.');
      }
    } finally {
      if (requestId === searchRequestId.current) {
        setIsSearching(false);
      }
    }
  }, []);

  // Summary polling management. Timers live in refs so these callbacks remain stable
  // across renders; meeting-details cleanup must not cancel a newly-started poll.
  const syncActiveSummaryPolls = React.useCallback(() => {
    setActiveSummaryPolls(new Map(activeSummaryPollsRef.current));
  }, []);

  const stopSummaryPolling = React.useCallback((meetingId: string) => {
    summaryPollGenerationsRef.current.set(
      meetingId,
      (summaryPollGenerationsRef.current.get(meetingId) ?? 0) + 1
    );

    const pollTimer = activeSummaryPollsRef.current.get(meetingId);
    if (pollTimer !== undefined) {
      console.log(`⏹️ Stopping polling for meeting ${meetingId}`);
      clearTimeout(pollTimer);
      activeSummaryPollsRef.current.delete(meetingId);
      syncActiveSummaryPolls();
    }
  }, [syncActiveSummaryPolls]);

  const startSummaryPolling = React.useCallback((
    meetingId: string,
    processId: string,
    onUpdate: (result: any, isCurrentPoll: () => boolean) => void | Promise<void>
  ) => {
    stopSummaryPolling(meetingId);

    const generation = (summaryPollGenerationsRef.current.get(meetingId) ?? 0) + 1;
    summaryPollGenerationsRef.current.set(meetingId, generation);

    const isCurrentPoll = () => (
      isSummaryPollingMountedRef.current &&
      summaryPollGenerationsRef.current.get(meetingId) === generation
    );
    const clearCurrentPoll = () => {
      if (!isCurrentPoll()) return;
      const timer = activeSummaryPollsRef.current.get(meetingId);
      if (timer !== undefined) clearTimeout(timer);
      activeSummaryPollsRef.current.delete(meetingId);
      syncActiveSummaryPolls();
    };

    console.log(`📊 Starting polling for meeting ${meetingId}, process ${processId}`);

    let pollCount = 0;
    const MAX_POLLS = 450;
    const poll = async () => {
      if (!isCurrentPoll()) return;
      pollCount++;

      if (getSummaryPollDecision('processing', pollCount, MAX_POLLS) === 'timeout') {
        await onUpdate({
          status: 'error',
          error: 'Summary generation timed out after 15 minutes. Please try again or check your model configuration.'
        }, isCurrentPoll);
        if (!isCurrentPoll()) return;
        clearCurrentPoll();
        return;
      }

      try {
        const result = await invoke('api_get_summary', { meetingId }) as any;
        if (!isCurrentPoll()) return;

        console.log(`📊 Polling update for ${meetingId}:`, result.status);
        await onUpdate(result, isCurrentPoll);
        if (!isCurrentPoll()) return;

        const decision = getSummaryPollDecision(result.status, pollCount, MAX_POLLS);
        if (decision === 'terminal') {
          clearCurrentPoll();
          return;
        }

        if (decision === 'missing') {
          await onUpdate({
            status: 'error',
            error: 'The summary process disappeared before a result was returned. Please try again.'
          }, isCurrentPoll);
          if (!isCurrentPoll()) return;
          clearCurrentPoll();
          return;
        }

        schedulePoll(2000);
      } catch (error) {
        if (isCurrentPoll()) {
          await onUpdate({
            status: 'error',
            error: error instanceof Error ? error.message : 'Unknown error'
          }, isCurrentPoll);
          if (!isCurrentPoll()) return;
          clearCurrentPoll();
        }
      }
    };

    const schedulePoll = (delay: number) => {
      if (!isCurrentPoll()) return;
      const timer = setTimeout(() => void poll(), delay);
      activeSummaryPollsRef.current.set(meetingId, timer);
      syncActiveSummaryPolls();
    };

    schedulePoll(0);
  }, [stopSummaryPolling, syncActiveSummaryPolls]);

  // Cleanup all polling intervals on unmount
  useEffect(() => {
    isSummaryPollingMountedRef.current = true;
    const pollTimers = activeSummaryPollsRef.current;
    const pollGenerations = summaryPollGenerationsRef.current;
    return () => {
      console.log('🧹 Cleaning up all summary polling intervals');
      isSummaryPollingMountedRef.current = false;
      pollGenerations.forEach((generation, meetingId) => {
        pollGenerations.set(meetingId, generation + 1);
      });
      pollTimers.forEach(timer => clearTimeout(timer));
      pollTimers.clear();
    };
  }, []);



  return (
    <SidebarContext.Provider value={{
      currentMeeting,
      setCurrentMeeting,
      sidebarItems,
      isCollapsed,
      toggleCollapse,
      meetings,
      setMeetings,
      isMeetingActive,
      setIsMeetingActive,
      handleRecordingToggle,
      searchTranscripts,
      searchResults,
      isSearching,
      searchError,
      setServerAddress,
      serverAddress,
      transcriptServerAddress,
      setTranscriptServerAddress,
      activeSummaryPolls,
      startSummaryPolling,
      stopSummaryPolling,
      refetchMeetings: fetchMeetings,

    }}>
      {children}
    </SidebarContext.Provider>
  );
}
