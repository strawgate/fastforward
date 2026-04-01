import { useState, useEffect, useCallback, useRef } from "preact/hooks";
import { api } from "./api";
import { RateTracker } from "./lib/rates";
import { RingBuffer } from "./lib/ring";
import { fmt, fmtBytes } from "./lib/format";
import type { PipelinesResponse, StatsResponse } from "./types";
import { StatusBar } from "./components/StatusBar";
import { SystemRow } from "./components/SystemRow";
import { MetricBadges } from "./components/MetricBadges";
import { ChartGrid } from "./components/ChartGrid";
import { PipelineView } from "./components/PipelineView";
import { ConfigView } from "./components/ConfigView";

const POLL_MS = 2000;

export interface MetricSeries {
  id: string;
  label: string;
  color: string;
  ring: RingBuffer;
  value: string;
  unit: string;
  limit?: string;
}

const rates = new RateTracker();

function createSeries(): MetricSeries[] {
  return [
    { id: "lps", label: "Lines / sec", color: "#3b82f6", ring: new RingBuffer(), value: "-", unit: "/s" },
    { id: "bps", label: "Input Bytes", color: "#8b5cf6", ring: new RingBuffer(), value: "-", unit: "/s" },
    { id: "err", label: "Errors / sec", color: "#ef4444", ring: new RingBuffer(), value: "-", unit: "/s" },
    { id: "cpu", label: "Process CPU", color: "#f59e0b", ring: new RingBuffer(), value: "-", unit: "%" },
    { id: "rss", label: "Memory (RSS)", color: "#10b981", ring: new RingBuffer(), value: "-", unit: "" },
    { id: "fds", label: "File Descriptors", color: "#06b6d4", ring: new RingBuffer(), value: "-", unit: "" },
  ];
}

export function App() {
  const [connected, setConnected] = useState(false);
  const [pipes, setPipes] = useState<PipelinesResponse | null>(null);
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [totalErrors, setTotalErrors] = useState(0);
  const seriesRef = useRef(createSeries());
  const [, forceUpdate] = useState(0);

  const poll = useCallback(async () => {
    const [pipeData, statsData] = await Promise.all([api.pipelines(), api.stats()]);

    if (pipeData) {
      setConnected(true);
      setPipes(pipeData);

      // Compute rates from pipeline counters
      let tl = 0, tb = 0, te = 0;
      for (const p of pipeData.pipelines) {
        tl += p.transform.lines_in;
        for (const i of p.inputs) tb += i.bytes_total;
        for (const o of p.outputs) te += o.errors;
      }
      setTotalErrors(te);

      const series = seriesRef.current;
      const lps = rates.rate("lps", tl);
      const bps = rates.rate("bps", tb);
      const eps = rates.rate("eps", te);

      if (lps != null) { series[0].ring.push(lps); series[0].value = fmt(lps); }
      if (bps != null) { series[1].ring.push(bps); series[1].value = fmtBytes(bps); }
      if (eps != null) { series[2].ring.push(eps); series[2].value = fmt(eps); }
    } else {
      setConnected(false);
    }

    if (statsData) {
      setStats(statsData);
      const series = seriesRef.current;

      // CPU: compute from user + sys ms deltas
      if (statsData.cpu_user_ms != null && statsData.cpu_sys_ms != null) {
        const cpuMs = statsData.cpu_user_ms + statsData.cpu_sys_ms;
        const cpuRate = rates.rate("cpu_ms", cpuMs);
        if (cpuRate != null) {
          const cpuPct = cpuRate / 10; // ms/s → %
          series[3].ring.push(cpuPct);
          series[3].value = cpuPct.toFixed(1);
        }
      }

      if (statsData.rss_bytes != null) {
        series[4].ring.push(statsData.rss_bytes);
        series[4].value = fmtBytes(statsData.rss_bytes);
        if (statsData.mem_resident) {
          series[4].limit = "/ " + fmtBytes(statsData.mem_resident) + " resident";
        }
      }
    }

    rates.tick();
    forceUpdate((n) => n + 1); // trigger re-render for charts
  }, []);

  useEffect(() => {
    poll();
    const id = setInterval(poll, POLL_MS);
    return () => clearInterval(id);
  }, [poll]);

  const version = pipes?.system?.version ?? stats?.uptime_sec != null ? "?" : "";
  const uptime = stats?.uptime_sec ?? pipes?.system?.uptime_seconds ?? 0;

  return (
    <>
      <StatusBar
        connected={connected}
        totalErrors={totalErrors}
        version={version}
        uptime={uptime}
      />
      <main>
        <SystemRow stats={stats} />
        <MetricBadges stats={stats} />

        <div class="section">
          <div class="heading">Metrics</div>
          <ChartGrid series={seriesRef.current} />
        </div>

        {pipes?.pipelines.map((p) => (
          <PipelineView key={p.name} pipeline={p} rates={rates} />
        ))}

        <ConfigView />
      </main>
    </>
  );
}
