import { describe, expect, test } from 'bun:test';
import { getSummaryPollDecision } from '../../src/lib/summary-polling';

describe('getSummaryPollDecision', () => {
  test('continues through active and first-idle states', () => {
    expect(getSummaryPollDecision('processing', 2, 450)).toBe('continue');
    expect(getSummaryPollDecision('idle', 1, 450)).toBe('continue');
  });

  test('terminates every backend terminal state', () => {
    for (const status of ['completed', 'error', 'failed', 'cancelled']) {
      expect(getSummaryPollDecision(status, 2, 450)).toBe('terminal');
    }
  });

  test('distinguishes a disappeared process from timeout', () => {
    expect(getSummaryPollDecision('idle', 2, 450)).toBe('missing');
    expect(getSummaryPollDecision('processing', 450, 450)).toBe('timeout');
  });
});
