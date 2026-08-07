import { useState, useCallback, useRef, useEffect, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Transcript, MeetingMetadata, PaginatedTranscriptsResponse, TranscriptSegmentData } from "@/types";
import { RequestGeneration } from "@/lib/request-generation";

const DEFAULT_PAGE_SIZE = 100;

interface UsePaginatedTranscriptsProps {
    meetingId: string | null;
}

interface UsePaginatedTranscriptsReturn {
    metadata: MeetingMetadata | null;
    segments: TranscriptSegmentData[];
    transcripts: Transcript[];
    isLoading: boolean;
    isLoadingMore: boolean;
    hasMore: boolean;
    totalCount: number;
    loadedCount: number;
    error: string | null;

    // Actions
    loadMore: () => Promise<void>;
    reset: () => void;
    refetch: () => Promise<void>;
}

/**
 * Convert Transcript array to TranscriptSegmentData for virtualized display
 */
function convertTranscriptsToSegments(transcripts: Transcript[]): TranscriptSegmentData[] {
    return transcripts.map(t => ({
        id: t.id,
        timestamp: t.audio_start_time ?? 0,
        endTime: t.audio_end_time,
        text: t.text,
        confidence: t.confidence,
        source: t.source,
        speakerId: t.speaker_id,
        speakerName: t.speaker_name,
    }));
}

export function usePaginatedTranscripts({
    meetingId,
}: UsePaginatedTranscriptsProps): UsePaginatedTranscriptsReturn {
    const [metadata, setMetadata] = useState<MeetingMetadata | null>(null);
    const [transcripts, setTranscripts] = useState<Transcript[]>([]);
    const [totalCount, setTotalCount] = useState(0);
    const [isLoading, setIsLoading] = useState(true);
    const [isLoadingMore, setIsLoadingMore] = useState(false);
    const [hasMore, setHasMore] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const offsetRef = useRef(0);
    const loadedMeetingIdRef = useRef<string | null>(null);
    const isLoadingRef = useRef(false);
    const lastLoadTimeRef = useRef(0); // Debounce protection
    const requestGenerationRef = useRef(new RequestGeneration());

    const resetState = useCallback(() => {
        setMetadata(null);
        setTranscripts([]);
        setTotalCount(0);
        setIsLoading(true);
        setIsLoadingMore(false);
        setHasMore(false);
        setError(null);
        offsetRef.current = 0;
        isLoadingRef.current = false;
        lastLoadTimeRef.current = 0;
    }, []);

    // Public reset also invalidates every in-flight native request.
    const reset = useCallback(() => {
        requestGenerationRef.current.invalidate();
        loadedMeetingIdRef.current = null;
        resetState();
    }, [resetState]);

    // Load meeting metadata
    const loadMetadata = useCallback(async (generation: number): Promise<MeetingMetadata | null> => {
        if (!meetingId) return null;

        try {
            const data = await invoke<MeetingMetadata>('api_get_meeting_metadata', {
                meetingId,
            });
            if (!requestGenerationRef.current.isCurrent(generation)) return null;
            setMetadata(data);
            return data;
        } catch (err) {
            if (!requestGenerationRef.current.isCurrent(generation)) return null;
            console.error('Failed to load meeting metadata:', err);
            setError('Failed to load meeting details');
            return null;
        }
    }, [meetingId]);

    // Load transcripts at specific offset
    const loadTranscriptsAtOffset = useCallback(async (
        offset: number,
        append: boolean,
        generation: number,
    ): Promise<Transcript[]> => {
        if (!meetingId) return [];

        try {
            const response = await invoke<PaginatedTranscriptsResponse>(
                'api_get_meeting_transcripts',
                {
                    meetingId,
                    limit: DEFAULT_PAGE_SIZE,
                    offset,
                }
            );
            if (!requestGenerationRef.current.isCurrent(generation)) return [];

            const newTranscripts = response.transcripts;

            if (append) {
                setTranscripts(prev => {
                    // Deduplicate by id
                    const existingIds = new Set(prev.map(t => t.id));
                    const uniqueNew = newTranscripts.filter(t => !existingIds.has(t.id));
                    // Sort by audio_start_time
                    return [...prev, ...uniqueNew].sort((a, b) =>
                        (a.audio_start_time ?? 0) - (b.audio_start_time ?? 0)
                    );
                });
            } else {
                setTranscripts(newTranscripts);
            }

            setHasMore(response.has_more);
            setTotalCount(response.total_count);
            offsetRef.current = offset + newTranscripts.length;

            return newTranscripts;
        } catch (err) {
            if (!requestGenerationRef.current.isCurrent(generation)) return [];
            console.error('Failed to load transcripts:', err);
            setError('Failed to load transcripts');
            return [];
        }
    }, [meetingId]);

    // Load next page with debounce protection
    const loadMore = useCallback(async () => {
        const now = Date.now();
        // Debounce: require at least 100ms between calls
        if (now - lastLoadTimeRef.current < 100) {
            return;
        }

        if (isLoadingRef.current || !hasMore || !meetingId || isLoading) return;

        lastLoadTimeRef.current = now;
        isLoadingRef.current = true;
        setIsLoadingMore(true);
        const generation = requestGenerationRef.current.begin();
        try {
            await loadTranscriptsAtOffset(offsetRef.current, true, generation);
        } finally {
            if (requestGenerationRef.current.isCurrent(generation)) {
                setIsLoadingMore(false);
                isLoadingRef.current = false;
            }
        }
    }, [hasMore, meetingId, loadTranscriptsAtOffset, isLoading]);

    // Force refetch of data (e.g., after retranscription)
    const refetch = useCallback(async () => {
        if (!meetingId) return;

        const requestGeneration = requestGenerationRef.current;
        const generation = requestGeneration.begin();
        resetState();
        setIsLoading(true);
        try {
            await loadMetadata(generation);
            if (!requestGeneration.isCurrent(generation)) return;
            await loadTranscriptsAtOffset(0, false, generation);
        } finally {
            if (requestGeneration.isCurrent(generation)) {
                setIsLoading(false);
            }
        }
    }, [meetingId, resetState, loadMetadata, loadTranscriptsAtOffset]);

    // Initial load
    useEffect(() => {
        if (!meetingId) {
            requestGenerationRef.current.invalidate();
            loadedMeetingIdRef.current = null;
            resetState();
            setIsLoading(false);
            return;
        }

        // Avoid reloading the same meeting
        if (loadedMeetingIdRef.current === meetingId) return;
        loadedMeetingIdRef.current = meetingId;

        const requestGeneration = requestGenerationRef.current;
        const generation = requestGeneration.begin();
        resetState();

        const loadInitial = async () => {
            setIsLoading(true);
            try {
                await loadMetadata(generation);
                if (!requestGeneration.isCurrent(generation)) return;
                await loadTranscriptsAtOffset(0, false, generation);
            } finally {
                if (requestGeneration.isCurrent(generation)) {
                    setIsLoading(false);
                }
            }
        };

        void loadInitial();

        return () => {
            requestGeneration.invalidate();
        };
    }, [meetingId, resetState, loadMetadata, loadTranscriptsAtOffset]);

    // Convert to segments (memoized)
    const segments = useMemo(() =>
        convertTranscriptsToSegments(transcripts),
        [transcripts]
    );

    return {
        metadata,
        segments,
        transcripts,
        isLoading,
        isLoadingMore,
        hasMore,
        totalCount,
        loadedCount: transcripts.length,
        error,
        loadMore,
        reset,
        refetch,
    };
}
