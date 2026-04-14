import type { MetricSeries } from "../app";
import type { MetricId } from "../types";

// ── Metric ordering constants ────────────────────────────────────────────────

export const PIPELINE_METRIC_ORDER: MetricId[] = [
  "lps",
  "bps",
  "obps",
  "err",
  "lat",
  "inflight",
  "batches",
  "stalls",
];

export const SYSTEM_METRIC_ORDER: MetricId[] = ["cpu", "mem"];

// ── MetricRegistry ───────────────────────────────────────────────────────────

export interface MetricRegistry {
  series: MetricSeries[];
  byId: Map<MetricId, MetricSeries>;
}

/** Wrap a series array into a registry with fast ID lookups. */
export function createMetricRegistry(series: MetricSeries[]): MetricRegistry {
  const byId = new Map<MetricId, MetricSeries>();
  for (const s of series) byId.set(s.id, s);
  return { series, byId };
}

/** Push a live metric sample (uses Date.now() as timestamp). */
export function pushMetricSample(
  reg: MetricRegistry,
  id: MetricId,
  value: number,
  formatted: string,
  limit?: string
) {
  const s = reg.byId.get(id);
  if (!s) return;
  s.ring.push(value);
  s.value = formatted;
  if (limit !== undefined) s.limit = limit;
}

/** Push a historical metric sample with an explicit epoch-ms timestamp. */
export function pushMetricHistorySample(
  reg: MetricRegistry,
  id: MetricId,
  timestampMs: number,
  value: number
) {
  const s = reg.byId.get(id);
  if (!s) return;
  s.ring.pushRaw(timestampMs, value);
}

/** Return series filtered and ordered by the given ID list. */
export function orderedMetrics(reg: MetricRegistry, order: MetricId[]): MetricSeries[] {
  return order.map((id) => reg.byId.get(id)).filter((s): s is MetricSeries => s != null);
}
