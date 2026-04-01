import { useEffect, useRef } from "preact/hooks";
import type { MetricSeries } from "../app";
import { fmtCompact, fmtBytesCompact } from "../lib/format";

const MAX_AGE = 5 * 60 * 1000;

interface Props {
  series: MetricSeries;
}

export function Chart({ series }: Props) {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    draw(canvasRef.current, series);
  });

  return <canvas ref={canvasRef} class="chart-canvas" />;
}

function draw(canvas: HTMLCanvasElement | null, series: MetricSeries) {
  if (!canvas || !canvas.offsetWidth) return;
  const ctx = canvas.getContext("2d")!;
  const dpr = devicePixelRatio || 1;
  const w = canvas.offsetWidth;
  const h = canvas.offsetHeight;
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  ctx.scale(dpr, dpr);

  const PL = 42, PR = 6, PB = 16, PT = 10;
  const cw = w - PL - PR;
  const ch = h - PT - PB;
  const now = Date.now();
  const data = series.ring.points(MAX_AGE);
  const fmtY = series.id === "bps" || series.id === "rss" ? fmtBytesCompact : fmtCompact;

  // Background
  ctx.fillStyle = "#0e1119";
  ctx.fillRect(0, 0, w, h);

  if (data.length < 2) {
    ctx.fillStyle = "#64748b";
    ctx.font = "10px sans-serif";
    ctx.textAlign = "center";
    ctx.fillText("waiting\u2026", w / 2, h / 2);
    return;
  }

  // Y range
  const vals = data.map((d) => d.v);
  let mn = 0;
  let mx = Math.max(...vals) * 1.2;
  if (mx < 0.001) mx = 1;
  if (mx <= mn) mx = mn + 1;

  // Time window
  const age = now - data[0].t;
  const win = Math.max(10000, Math.min(age, MAX_AGE));
  const t0 = now - win;
  const tx = (t: number) => PL + Math.max(0, ((t - t0) / win) * cw);
  const ty = (v: number) => PT + ch - ((v - mn) / (mx - mn)) * ch;

  const mono = getComputedStyle(document.body).getPropertyValue("--mono") || "monospace";
  const labelFont = `9px ${mono}`;

  // Y grid + labels
  ctx.font = labelFont;
  ctx.textAlign = "right";
  ctx.textBaseline = "middle";
  for (let g = 0; g <= 3; g++) {
    const f = g / 3;
    const gy = PT + ch - ch * f;
    ctx.strokeStyle = "#1e2536";
    ctx.lineWidth = 0.5;
    ctx.beginPath();
    ctx.moveTo(PL, gy);
    ctx.lineTo(w - PR, gy);
    ctx.stroke();
    ctx.fillStyle = "#94a3b8";
    ctx.fillText(fmtY(mn + (mx - mn) * f), PL - 4, gy);
  }

  // X labels
  ctx.textAlign = "center";
  ctx.textBaseline = "top";
  ctx.fillStyle = "#94a3b8";
  ctx.font = labelFont;
  for (const s of [10, 30, 60, 120, 300]) {
    const x = tx(now - s * 1000);
    if (x > PL + 18 && x < w - PR - 30) {
      ctx.fillText(s < 60 ? `${s}s` : `${Math.floor(s / 60)}m`, x, PT + ch + 3);
    }
  }
  ctx.textAlign = "right";
  ctx.fillText("now", w - PR, PT + ch + 3);

  // Line
  ctx.beginPath();
  for (let i = 0; i < data.length; i++) {
    const x = tx(data[i].t);
    const y = Math.max(PT, Math.min(PT + ch, ty(data[i].v)));
    i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
  }
  ctx.strokeStyle = series.color;
  ctx.lineWidth = 1.5;
  ctx.stroke();

  // Fill
  const last = tx(data[data.length - 1].t);
  const first = tx(data[0].t);
  ctx.lineTo(last, PT + ch);
  ctx.lineTo(first, PT + ch);
  ctx.closePath();
  const grad = ctx.createLinearGradient(0, PT, 0, PT + ch);
  grad.addColorStop(0, series.color + "28");
  grad.addColorStop(1, series.color + "04");
  ctx.fillStyle = grad;
  ctx.fill();
}
