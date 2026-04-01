// API response types from logfwd diagnostics server

export interface ComponentData {
  name: string;
  type: string;
  lines_total: number;
  bytes_total: number;
  errors: number;
  parse_errors?: number;
}

export interface TransformData {
  sql: string;
  lines_in: number;
  lines_out: number;
  filter_drop_rate: number;
}

export interface PipelineData {
  name: string;
  inputs: ComponentData[];
  transform: TransformData;
  outputs: ComponentData[];
  batches?: number;
  scan_sec?: number;
  transform_sec?: number;
  output_sec?: number;
  backpressure_stalls?: number;
}

export interface PipelinesResponse {
  pipelines: PipelineData[];
  system: {
    uptime_seconds: number;
    version: string;
  };
}

export interface StatsResponse {
  uptime_sec: number;
  rss_bytes: number | null;
  cpu_user_ms: number | null;
  cpu_sys_ms: number | null;
  input_lines: number;
  input_bytes: number;
  output_lines: number;
  output_bytes: number;
  output_errors: number;
  batches: number;
  scan_sec: number;
  transform_sec: number;
  output_sec: number;
  backpressure_stalls: number;
  mem_resident?: number;
  mem_allocated?: number;
  mem_active?: number;
}

export interface ConfigResponse {
  path: string;
  raw_yaml: string;
}

export interface SystemResponse {
  uptime_seconds: number;
  version: string;
  pid: number;
  cpu_percent: number | null;
  global_cpu_percent: number | null;
  cpu_count: number;
  rss_bytes: number;
  virtual_bytes: number;
  total_memory_bytes: number;
  available_memory_bytes: number;
  used_memory_bytes: number;
  total_swap_bytes: number;
  used_swap_bytes: number;
  fd_count: number | null;
  fd_limit: number | null;
  disk_read_bytes: number;
  disk_write_bytes: number;
  cgroup_memory_limit: number | null;
  hostname: string;
  os: string;
  arch: string;
}
