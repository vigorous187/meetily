import { describe, expect, test } from 'bun:test';
import {
  isSummaryProcessingStatus,
  normalizeMeetingSummary,
} from '../../src/lib/meeting-summary';

describe('normalizeMeetingSummary', () => {
  test('preserves markdown and BlockNote JSON together', () => {
    const blocks = [{ id: 'one', type: 'paragraph', content: [] }];

    expect(normalizeMeetingSummary({ markdown: '# Notes', summary_json: blocks })).toEqual({
      markdown: '# Notes',
      summary_json: blocks,
    });
  });

  test('decodes historical double-encoded JSON', () => {
    const encoded = JSON.stringify(JSON.stringify({ markdown: 'Recovered' }));
    expect(normalizeMeetingSummary(encoded)).toEqual({ markdown: 'Recovered' });
  });

  test('normalizes legacy sections in declared order and ignores metadata', () => {
    const normalized = normalizeMeetingSummary({
      MeetingName: 'Planning',
      _section_order: ['decisions', 'topics', 'decisions'],
      topics: {
        title: 'Topics',
        blocks: [{ id: 'topic', type: 'bullet', color: 'red', content: '  Roadmap  ' }],
      },
      decisions: {
        title: '',
        blocks: [{ id: 'decision', type: 'bullet', content: 'Ship it' }],
      },
    });

    expect(Object.keys(normalized ?? {})).toEqual(['decisions', 'topics']);
    expect(normalized).toEqual({
      decisions: {
        title: 'decisions',
        blocks: [{ id: 'decision', type: 'bullet', color: 'default', content: 'Ship it' }],
      },
      topics: {
        title: 'Topics',
        blocks: [{ id: 'topic', type: 'bullet', color: 'default', content: 'Roadmap' }],
      },
    });
  });

  test('rejects malformed and empty payloads', () => {
    expect(normalizeMeetingSummary('{not-json')).toBeNull();
    expect(normalizeMeetingSummary({})).toBeNull();
    expect(normalizeMeetingSummary({ markdown: '   ', summary_json: [] })).toBeNull();
    expect(normalizeMeetingSummary({ notes: { title: 'Notes', blocks: [] } })).toBeNull();
  });
});

describe('isSummaryProcessingStatus', () => {
  test('recognizes every resumable backend state', () => {
    for (const status of ['pending', 'processing', 'summarizing', 'regenerating']) {
      expect(isSummaryProcessingStatus(status)).toBe(true);
    }
    expect(isSummaryProcessingStatus('completed')).toBe(false);
    expect(isSummaryProcessingStatus('idle')).toBe(false);
  });
});
