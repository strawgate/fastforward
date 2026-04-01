import type { StatsResponse } from "../types";
import { fmtBytes } from "../lib/format";

interface Props {
  stats: StatsResponse | null;
}

export function SystemRow({ stats }: Props) {
  if (!stats) return null;

  return (
    <div class="section">
      <div class="heading">System</div>
      <div class="sys-row">
        {stats.rss_bytes != null && (
          <span>RSS <b>{fmtBytes(stats.rss_bytes)}</b></span>
        )}
        {stats.mem_allocated != null && (
          <span>Heap <b>{fmtBytes(stats.mem_allocated)}</b></span>
        )}
        {stats.batches > 0 && (
          <span>Batches <b>{stats.batches.toLocaleString()}</b></span>
        )}
      </div>
    </div>
  );
}
