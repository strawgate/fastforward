import type { OtlpTracesDocument } from "@otlpkit/otlpjson";
import { describe, expect, it } from "vitest";
import { extractTraceRecords } from "../lib/otlpProcess";

// ---------------------------------------------------------------------------
// Helpers to build OTLP JSON documents
// ---------------------------------------------------------------------------

function tracesDoc(spans: Array<Record<string, unknown>>): OtlpTracesDocument {
  return {
    resourceSpans: [
      {
        resource: { attributes: [] },
        scopeSpans: [
          {
            scope: { name: "logfwd.diagnostics" },
            spans,
          },
        ],
      },
    ],
  } as unknown as OtlpTracesDocument;
}

function rootSpan(overrides: Record<string, unknown> = {}) {
  return {
    traceId: "aaaa0000bbbb1111cccc2222dddd3333",
    spanId: "0000000000000001",
    parentSpanId: "",
    name: "batch",
    startTimeUnixNano: "1000000000",
    endTimeUnixNano: "2000000000",
    status: { code: 0 },
    attributes: [],
    ...overrides,
  };
}

function childSpan(name: string, overrides: Record<string, unknown> = {}) {
  return {
    traceId: "aaaa0000bbbb1111cccc2222dddd3333",
    spanId: "0000000000000002",
    parentSpanId: "0000000000000001",
    name,
    startTimeUnixNano: "1100000000",
    endTimeUnixNano: "1500000000",
    status: { code: 0 },
    attributes: [],
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// extractTraceRecords
// ---------------------------------------------------------------------------

describe("extractTraceRecords", () => {
  it("extracts a completed root span", () => {
    const doc = tracesDoc([
      rootSpan({
        attributes: [
          { key: "pipeline", value: { stringValue: "main" } },
          { key: "input_rows", value: { intValue: "50" } },
          { key: "output_rows", value: { intValue: "48" } },
          { key: "bytes_in", value: { intValue: "1024" } },
          { key: "errors", value: { intValue: "0" } },
          { key: "flush_reason", value: { stringValue: "size" } },
        ],
      }),
    ]);

    const records = extractTraceRecords(doc);
    expect(records).toHaveLength(1);
    expect(records[0].trace_id).toBe("aaaa0000bbbb1111cccc2222dddd3333");
    expect(records[0].pipeline).toBe("main");
    expect(records[0].lifecycle_state).toBe("completed");
    expect(records[0].total_ns).toBe("1000000000");
    expect(records[0].flush_reason).toBe("size");
  });

  it("detects in-progress spans with stage", () => {
    const doc = tracesDoc([
      rootSpan({
        endTimeUnixNano: "0",
        attributes: [
          { key: "pipeline", value: { stringValue: "main" } },
          { key: "in_progress", value: { boolValue: true } },
          { key: "stage", value: { stringValue: "output" } },
          { key: "stage_start_unix_ns", value: { stringValue: "1200000000" } },
        ],
      }),
    ]);

    const records = extractTraceRecords(doc);
    expect(records).toHaveLength(1);
    expect(records[0].lifecycle_state).toBe("output_in_progress");
    expect(records[0].lifecycle_state_start_unix_ns).toBe("1200000000");
  });

  it("reconstructs child spans into root TraceRecord", () => {
    const doc = tracesDoc([
      rootSpan({
        attributes: [{ key: "pipeline", value: { stringValue: "main" } }],
      }),
      childSpan("scan", {
        startTimeUnixNano: "1000000000",
        endTimeUnixNano: "1200000000",
        attributes: [{ key: "rows", value: { intValue: "50" } }],
      }),
      childSpan("transform", {
        spanId: "0000000000000003",
        startTimeUnixNano: "1200000000",
        endTimeUnixNano: "1300000000",
      }),
      childSpan("output", {
        spanId: "0000000000000004",
        startTimeUnixNano: "1300000000",
        endTimeUnixNano: "1800000000",
        attributes: [
          { key: "worker_id", value: { intValue: "2" } },
          { key: "took_ms", value: { intValue: "45" } },
          { key: "req_bytes", value: { intValue: "512" } },
          { key: "cmp_bytes", value: { intValue: "256" } },
          { key: "resp_bytes", value: { intValue: "64" } },
          { key: "retries", value: { intValue: "1" } },
        ],
      }),
    ]);

    const records = extractTraceRecords(doc);
    expect(records).toHaveLength(1);

    const r = records[0];
    expect(r.scan_ns).toBe("200000000");
    expect(r.scan_rows).toBe(50);
    expect(r.transform_ns).toBe("100000000");
    expect(r.output_ns).toBe("500000000");
    expect(r.worker_id).toBe(2);
    expect(r.took_ms).toBe(45);
    expect(r.req_bytes).toBe(512);
    expect(r.cmp_bytes).toBe(256);
    expect(r.resp_bytes).toBe(64);
    expect(r.retries).toBe(1);
  });

  it("falls back to root attributes when no child spans", () => {
    const doc = tracesDoc([
      rootSpan({
        attributes: [
          { key: "pipeline", value: { stringValue: "main" } },
          { key: "scan_ns", value: { intValue: "150000000" } },
          { key: "transform_ns", value: { intValue: "50000000" } },
        ],
      }),
    ]);

    const records = extractTraceRecords(doc);
    expect(records[0].scan_ns).toBe("150000000");
    expect(records[0].transform_ns).toBe("50000000");
  });

  it("handles error status", () => {
    const doc = tracesDoc([
      rootSpan({
        status: { code: 2, message: "output failed" },
        attributes: [
          { key: "pipeline", value: { stringValue: "main" } },
          { key: "errors", value: { intValue: "1" } },
        ],
      }),
    ]);

    const records = extractTraceRecords(doc);
    expect(records[0].status).toBe("error");
    expect(records[0].errors).toBe(1);
  });

  it("handles empty traces document", () => {
    const doc = tracesDoc([]);
    expect(extractTraceRecords(doc)).toEqual([]);
  });

  it("handles multiple root spans", () => {
    const doc = tracesDoc([
      rootSpan({
        traceId: "aaaa0000000000000000000000000001",
        attributes: [{ key: "pipeline", value: { stringValue: "p1" } }],
      }),
      rootSpan({
        traceId: "aaaa0000000000000000000000000002",
        spanId: "0000000000000099",
        attributes: [{ key: "pipeline", value: { stringValue: "p2" } }],
      }),
    ]);

    const records = extractTraceRecords(doc);
    expect(records).toHaveLength(2);
    expect(records[0].pipeline).toBe("p1");
    expect(records[1].pipeline).toBe("p2");
  });
});
