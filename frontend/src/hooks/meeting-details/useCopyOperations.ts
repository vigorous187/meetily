import { useCallback, RefObject } from 'react';
import { Transcript, Summary } from '@/types';
import { BlockNoteSummaryViewRef } from '@/components/AISummary/BlockNoteSummaryView';
import { toast } from 'sonner';
import Analytics from '@/lib/analytics';
import { invoke as invokeTauri } from '@tauri-apps/api/core';

const formatTranscriptTime = (
  seconds: number | undefined,
  fallbackTimestamp: string,
): string => {
  if (seconds === undefined) {
    return fallbackTimestamp;
  }

  const totalSeconds = Math.floor(seconds);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const remainingSeconds = totalSeconds % 60;

  if (hours > 0) {
    return `[${hours.toString().padStart(2, '0')}:${minutes
      .toString()
      .padStart(2, '0')}:${remainingSeconds.toString().padStart(2, '0')}]`;
  }

  return `[${minutes.toString().padStart(2, '0')}:${remainingSeconds
    .toString()
    .padStart(2, '0')}]`;
};

const formatTranscriptSpeaker = (transcript: Transcript): string | null => {
  if (transcript.speaker_name?.trim()) {
    return transcript.speaker_name.trim();
  }
  if (transcript.source === 'mic') {
    return 'You';
  }
  if (transcript.source === 'system') {
    return 'Remote speaker';
  }
  return null;
};

const formatTranscriptMarkdownLine = (transcript: Transcript): string => {
  const time = formatTranscriptTime(transcript.audio_start_time, transcript.timestamp);
  const speaker = formatTranscriptSpeaker(transcript);
  return speaker
    ? `${time} **${speaker}:** ${transcript.text}`
    : `${time} ${transcript.text}`;
};

interface UseCopyOperationsProps {
  meeting: any;
  transcripts: Transcript[];
  meetingTitle: string;
  aiSummary: Summary | null;
  blockNoteSummaryRef: RefObject<BlockNoteSummaryViewRef>;
}

export function useCopyOperations({
  meeting,
  transcripts,
  meetingTitle,
  aiSummary,
  blockNoteSummaryRef,
}: UseCopyOperationsProps) {

  // Helper function to fetch ALL transcripts for copying (not just paginated data)
  const fetchAllTranscripts = useCallback(async (meetingId: string): Promise<Transcript[]> => {
    try {
      console.log('📊 Fetching all transcripts for copying:', meetingId);

      // First, get total count by fetching first page
      const firstPage = await invokeTauri('api_get_meeting_transcripts', {
        meetingId,
        limit: 1,
        offset: 0,
      }) as { transcripts: Transcript[]; total_count: number; has_more: boolean };

      const totalCount = firstPage.total_count;
      console.log(`📊 Total transcripts in database: ${totalCount}`);

      if (totalCount === 0) {
        return [];
      }

      // Fetch all transcripts in one call
      const allData = await invokeTauri('api_get_meeting_transcripts', {
        meetingId,
        limit: totalCount,
        offset: 0,
      }) as { transcripts: Transcript[]; total_count: number; has_more: boolean };

      console.log(`✅ Fetched ${allData.transcripts.length} transcripts from database for copying`);
      return allData.transcripts;
    } catch (error) {
      console.error('❌ Error fetching all transcripts:', error);
      toast.error('Failed to fetch transcripts for copying');
      return [];
    }
  }, []);

  const getSummaryMarkdown = useCallback(async (): Promise<string> => {
    let summaryMarkdown = '';

    if (blockNoteSummaryRef.current?.getMarkdown) {
      summaryMarkdown = await blockNoteSummaryRef.current.getMarkdown();
    }

    if (!summaryMarkdown && aiSummary && 'markdown' in aiSummary) {
      summaryMarkdown = (aiSummary as any).markdown || '';
    }

    if (!summaryMarkdown && aiSummary) {
      summaryMarkdown = Object.entries(aiSummary)
        .filter(([key]) => (
          key !== 'markdown' &&
          key !== 'summary_json' &&
          key !== '_section_order' &&
          key !== 'MeetingName'
        ))
        .map(([, section]) => {
          if (section && typeof section === 'object' && 'title' in section && 'blocks' in section) {
            const sectionTitle = `## ${(section as any).title}\n\n`;
            const sectionContent = (section as any).blocks
              .map((block: any) => `- ${block.content}`)
              .join('\n');
            return sectionTitle + sectionContent;
          }
          return '';
        })
        .filter(section => section.trim())
        .join('\n\n');
    }

    return summaryMarkdown.trim();
  }, [aiSummary, blockNoteSummaryRef]);

  // Copy transcript to clipboard
  const handleCopyTranscript = useCallback(async () => {
    // CHANGE: Fetch ALL transcripts from database, not from pagination state
    console.log('📊 Fetching all transcripts for copying...');
    const allTranscripts = await fetchAllTranscripts(meeting.id);

    if (!allTranscripts.length) {
      const error_msg = 'No transcripts available to copy';
      console.log(error_msg);
      toast.error(error_msg);
      return;
    }

    console.log(`✅ Copying ${allTranscripts.length} transcripts to clipboard`);

    const header = `# Transcript of the Meeting: ${meeting.id} - ${meetingTitle ?? meeting.title}\n\n`;
    const date = `## Date: ${new Date(meeting.created_at).toLocaleDateString()}\n\n`;
    const fullTranscript = allTranscripts
      .map(t => `${formatTranscriptMarkdownLine(t)}  `)
      .join('\n');

    await navigator.clipboard.writeText(header + date + fullTranscript);
    toast.success("Transcript copied to clipboard");

    // Track copy analytics
    const wordCount = allTranscripts
      .map(t => t.text.split(/\s+/).length)
      .reduce((a, b) => a + b, 0);

    await Analytics.trackCopy('transcript', {
      meeting_id: meeting.id,
      transcript_length: allTranscripts.length.toString(),
      word_count: wordCount.toString()
    });
  }, [meeting, meetingTitle, fetchAllTranscripts]);

  // Copy summary to clipboard
  const handleCopySummary = useCallback(async () => {
    try {
      console.log('🔍 Copy Summary - Starting...');
      const summaryMarkdown = await getSummaryMarkdown();

      // If still no summary content, show message
      if (!summaryMarkdown.trim()) {
        console.error('❌ No summary content available to copy');
        toast.error('No summary content available to copy');
        return;
      }

      // Build metadata header
      const header = `# Meeting Summary: ${meetingTitle}\n\n`;
      const metadata = `**Meeting ID:** ${meeting.id}\n**Date:** ${new Date(meeting.created_at).toLocaleDateString('en-US', {
        year: 'numeric',
        month: 'long',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit'
      })}\n**Copied on:** ${new Date().toLocaleDateString('en-US', {
        year: 'numeric',
        month: 'long',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit'
      })}\n\n---\n\n`;

      const fullMarkdown = header + metadata + summaryMarkdown;
      await navigator.clipboard.writeText(fullMarkdown);

      console.log('✅ Successfully copied to clipboard!');
      toast.success("Summary copied to clipboard");

      // Track copy analytics
      await Analytics.trackCopy('summary', {
        meeting_id: meeting.id,
        has_markdown: (!!aiSummary && 'markdown' in aiSummary).toString()
      });
    } catch (error) {
      console.error('❌ Failed to copy summary:', error);
      toast.error("Failed to copy summary");
    }
  }, [aiSummary, meetingTitle, meeting, getSummaryMarkdown]);

  const handleExportMeeting = useCallback(async () => {
    try {
      const [allTranscripts, summaryMarkdown] = await Promise.all([
        fetchAllTranscripts(meeting.id),
        getSummaryMarkdown(),
      ]);

      if (!allTranscripts.length && !summaryMarkdown) {
        toast.error('No meeting content available to export');
        return;
      }

      const title = meetingTitle || meeting.title || 'Meeting notes';
      const meetingDate = new Date(meeting.created_at);
      const dateLabel = Number.isNaN(meetingDate.getTime())
        ? String(meeting.created_at)
        : meetingDate.toLocaleString();
      const exportedAt = new Date().toISOString();
      const transcriptMarkdown = allTranscripts
        .map(formatTranscriptMarkdownLine)
        .join('\n\n');

      const sections = [
        `# ${title}`,
        [
          `**Date:** ${dateLabel}`,
          `**Meeting ID:** ${meeting.id}`,
          `**Exported:** ${exportedAt}`,
        ].join('\n\n'),
      ];

      if (summaryMarkdown) {
        sections.push(`## Summary\n\n${summaryMarkdown}`);
      }

      if (transcriptMarkdown) {
        sections.push(`## Transcript\n\n${transcriptMarkdown}`);
      }

      await invokeTauri<string>('export_meeting_markdown', {
        directoryPath: null,
        suggestedName: title,
        content: `${sections.join('\n\n---\n\n')}\n`,
      });

      toast.success('Meeting exported as Markdown', {
        description: 'Saved to your Downloads or Documents folder',
      });
    } catch (error) {
      console.error('Failed to export meeting:', error);
      toast.error('Failed to export meeting', {
        description: String(error),
      });
    }
  }, [
    fetchAllTranscripts,
    getSummaryMarkdown,
    meeting,
    meetingTitle,
  ]);

  return {
    handleCopyTranscript,
    handleCopySummary,
    handleExportMeeting,
  };
}
