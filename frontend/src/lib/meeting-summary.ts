import type { BlockNoteBlock, Summary, SummaryDataResponse } from '@/types';

export type NormalizedMeetingSummary = Summary | SummaryDataResponse;

const SUMMARY_METADATA_KEYS = new Set([
  'MeetingName',
  '_section_order',
  'english_cache',
]);

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function decodeSummaryData(value: unknown): unknown {
  let decoded = value;

  // Historical rows can contain JSON encoded more than once. Keep the bound
  // small so malformed/self-referential input cannot loop forever.
  for (let depth = 0; depth < 3 && typeof decoded === 'string'; depth += 1) {
    const candidate = decoded.trim();
    if (!candidate) return null;

    try {
      decoded = JSON.parse(candidate);
    } catch {
      return null;
    }
  }

  return decoded;
}

export function normalizeMeetingSummary(value: unknown): NormalizedMeetingSummary | null {
  const decoded = decodeSummaryData(value);
  if (!isRecord(decoded)) return null;

  const markdown = typeof decoded.markdown === 'string' && decoded.markdown.trim()
    ? decoded.markdown
    : undefined;
  const summaryJson = Array.isArray(decoded.summary_json)
    ? decoded.summary_json as BlockNoteBlock[]
    : undefined;

  if (markdown || (summaryJson && summaryJson.length > 0)) {
    return {
      ...(markdown ? { markdown } : {}),
      ...(summaryJson ? { summary_json: summaryJson } : {}),
    };
  }

  const requestedOrder = Array.isArray(decoded._section_order)
    ? decoded._section_order.filter((key): key is string => typeof key === 'string')
    : [];
  const sectionKeys = [
    ...requestedOrder,
    ...Object.keys(decoded).filter((key) => !requestedOrder.includes(key)),
  ];
  const seen = new Set<string>();
  const normalized: Summary = {};

  for (const key of sectionKeys) {
    if (seen.has(key) || SUMMARY_METADATA_KEYS.has(key)) continue;
    seen.add(key);

    const section = decoded[key];
    if (!isRecord(section) || !Array.isArray(section.blocks)) continue;

    normalized[key] = {
      title: typeof section.title === 'string' && section.title.trim()
        ? section.title
        : key,
      blocks: section.blocks.map((block) => {
        const blockRecord = isRecord(block) ? block : {};
        return {
          id: typeof blockRecord.id === 'string' ? blockRecord.id : '',
          type: typeof blockRecord.type === 'string' ? blockRecord.type : 'bullet',
          color: 'default',
          content: typeof blockRecord.content === 'string'
            ? blockRecord.content.trim()
            : '',
        };
      }),
    };
  }

  return Object.values(normalized).some((section) => section.blocks.length > 0)
    ? normalized
    : null;
}

export function isSummaryProcessingStatus(status: unknown): boolean {
  return status === 'pending' ||
    status === 'processing' ||
    status === 'summarizing' ||
    status === 'regenerating';
}
