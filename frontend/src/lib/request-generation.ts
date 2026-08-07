/**
 * Small monotonic guard for asynchronous work tied to a changing owner, such
 * as a meeting id. Starting or invalidating a generation makes every older
 * completion stale without needing to cancel the underlying native command.
 */
export class RequestGeneration {
  private currentGeneration = 0;

  begin(): number {
    this.currentGeneration += 1;
    return this.currentGeneration;
  }

  invalidate(): void {
    this.currentGeneration += 1;
  }

  isCurrent(generation: number): boolean {
    return generation === this.currentGeneration;
  }
}
