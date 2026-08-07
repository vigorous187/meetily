import { describe, expect, test } from 'bun:test';
import { RequestGeneration } from '../../src/lib/request-generation';

describe('RequestGeneration', () => {
  test('only accepts the newest request generation', () => {
    const guard = new RequestGeneration();
    const meetingA = guard.begin();
    const meetingB = guard.begin();

    expect(guard.isCurrent(meetingA)).toBe(false);
    expect(guard.isCurrent(meetingB)).toBe(true);
  });

  test('invalidates in-flight work during cleanup', () => {
    const guard = new RequestGeneration();
    const request = guard.begin();
    guard.invalidate();
    expect(guard.isCurrent(request)).toBe(false);
  });
});
