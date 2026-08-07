export type SummaryPollDecision = 'continue' | 'terminal' | 'missing' | 'timeout';

export function getSummaryPollDecision(
  status: unknown,
  pollCount: number,
  maxPolls: number,
): SummaryPollDecision {
  if (pollCount >= maxPolls) return 'timeout';
  if (status === 'completed' || status === 'error' || status === 'failed' || status === 'cancelled') {
    return 'terminal';
  }
  if (status === 'idle' && pollCount > 1) return 'missing';
  return 'continue';
}
