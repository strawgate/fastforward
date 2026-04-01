/** EMA-smoothed rate tracker. Tracks counter deltas over time. */
export class RateTracker {
  private prev = new Map<string, number>();
  private ema = new Map<string, number>();
  private prevTime: number | null = null;
  private alpha: number;

  constructor(alpha: number = 0.3) {
    this.alpha = alpha;
  }

  /** Record a counter value. Returns the smoothed rate (units/sec) or null if first sample. */
  rate(key: string, value: number): number | null {
    const now = Date.now();
    const prev = this.prev.get(key);
    const dt = this.prevTime != null ? (now - this.prevTime) / 1000 : 0;
    this.prev.set(key, value);

    if (prev == null || dt <= 0) return null;

    const raw = Math.max(0, (value - prev) / dt);
    const prevEma = this.ema.get(key);
    const smoothed = prevEma == null ? raw : prevEma * (1 - this.alpha) + raw * this.alpha;
    this.ema.set(key, smoothed);
    return smoothed;
  }

  /** Call once per poll cycle after all rate() calls */
  tick() {
    this.prevTime = Date.now();
  }
}
