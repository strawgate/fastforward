//! Cross-platform beta sensor input.
//!
//! This source emits platform-gated beta telemetry snapshots for core families
//! (`process`, `network`, `disk_io`) plus control-plane lifecycle events.

use std::ffi::OsStr;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow::array::{ArrayRef, BooleanArray, Float32Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use logfwd_types::diagnostics::ComponentHealth;
use sysinfo::{Networks, Process, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::input::{InputEvent, InputSource};

/// Platform target for a beta sensor input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformSensorTarget {
    Linux,
    Macos,
    Windows,
}

impl PlatformSensorTarget {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
        }
    }
}

/// Core beta sensor event families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformSensorFamily {
    Process,
    Network,
    DiskIo,
}

impl PlatformSensorFamily {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Network => "network",
            Self::DiskIo => "disk_io",
        }
    }
}

/// Runtime options for platform beta inputs.
#[derive(Debug, Clone)]
pub struct PlatformSensorBetaConfig {
    /// Emit periodic heartbeats when no data rows were emitted in a cycle.
    pub emit_heartbeat: bool,
    /// Snapshot cadence.
    pub poll_interval: Duration,
    /// Families to collect each cycle.
    pub families: Vec<PlatformSensorFamily>,
    /// Upper bound on data rows emitted per cycle across all families.
    pub max_rows_per_poll: usize,
}

impl Default for PlatformSensorBetaConfig {
    fn default() -> Self {
        Self {
            emit_heartbeat: true,
            poll_interval: Duration::from_millis(10_000),
            families: vec![
                PlatformSensorFamily::Process,
                PlatformSensorFamily::Network,
                PlatformSensorFamily::DiskIo,
            ],
            max_rows_per_poll: 256,
        }
    }
}

/// Beta input source for per-platform sensor bring-up.
#[derive(Debug)]
pub struct PlatformSensorBetaInput {
    machine: Option<PlatformSensorMachineState>,
}

#[derive(Debug)]
enum PlatformSensorMachineState {
    Init(PlatformSensorMachine<InitState>),
    Running(PlatformSensorMachine<RunningState>),
}

#[derive(Debug)]
struct PlatformSensorMachine<S> {
    common: PlatformSensorCommon,
    state: S,
}

#[derive(Debug)]
struct PlatformSensorCommon {
    name: String,
    target: PlatformSensorTarget,
    host_platform: &'static str,
    cfg: PlatformSensorBetaConfig,
    system: System,
    networks: Networks,
}

#[derive(Debug)]
struct InitState;

#[derive(Debug)]
struct RunningState {
    last_cycle: Instant,
}

#[derive(Debug, Clone)]
struct SensorRow {
    timestamp_unix_nano: u64,
    level: String,
    event_family: String,
    sensor_event: String,
    sensor_status: String,
    sensor_target_platform: String,
    sensor_host_platform: String,
    sensor_name: String,
    sensor_beta: bool,

    message: Option<String>,
    sensor_family: Option<String>,
    sensor_family_state: Option<String>,

    process_pid: Option<u32>,
    process_parent_pid: Option<u32>,
    process_name: Option<String>,
    process_cmd: Option<String>,
    process_exe: Option<String>,
    process_status: Option<String>,
    process_cpu_usage: Option<f32>,
    process_memory_bytes: Option<u64>,
    process_virtual_memory_bytes: Option<u64>,
    process_start_time_unix_sec: Option<u64>,
    process_disk_read_delta_bytes: Option<u64>,
    process_disk_write_delta_bytes: Option<u64>,

    network_interface: Option<String>,
    network_received_delta_bytes: Option<u64>,
    network_transmitted_delta_bytes: Option<u64>,
    network_received_total_bytes: Option<u64>,
    network_transmitted_total_bytes: Option<u64>,
    network_packets_received_delta: Option<u64>,
    network_packets_transmitted_delta: Option<u64>,
    network_errors_received_delta: Option<u64>,
    network_errors_transmitted_delta: Option<u64>,

    disk_io_read_delta_bytes: Option<u64>,
    disk_io_write_delta_bytes: Option<u64>,
    disk_io_read_total_bytes: Option<u64>,
    disk_io_write_total_bytes: Option<u64>,
}

impl PlatformSensorBetaInput {
    /// Create a beta platform sensor source.
    ///
    /// Returns an error when `target` does not match the current host platform.
    pub fn new(
        name: impl Into<String>,
        target: PlatformSensorTarget,
        mut cfg: PlatformSensorBetaConfig,
    ) -> io::Result<Self> {
        let host_platform = current_host_platform().as_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "platform sensor beta inputs are unsupported on this host",
            )
        })?;

        if target.as_str() != host_platform {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} sensor beta input can only run on {} hosts (current host: {})",
                    target.as_str(),
                    target.as_str(),
                    host_platform
                ),
            ));
        }

        if cfg.families.is_empty() {
            cfg.families = PlatformSensorBetaConfig::default().families;
        }
        if cfg.max_rows_per_poll == 0 {
            cfg.max_rows_per_poll = 1;
        }

        Ok(Self {
            machine: Some(PlatformSensorMachineState::Init(PlatformSensorMachine {
                common: PlatformSensorCommon {
                    name: name.into(),
                    target,
                    host_platform,
                    cfg,
                    system: System::new_all(),
                    networks: Networks::new_with_refreshed_list(),
                },
                state: InitState,
            })),
        })
    }
}

impl PlatformSensorCommon {
    fn base_row(&self, event_family: &str, sensor_event: &str) -> SensorRow {
        SensorRow {
            timestamp_unix_nano: now_unix_nano(),
            level: "INFO".to_string(),
            event_family: event_family.to_string(),
            sensor_event: sensor_event.to_string(),
            sensor_status: "beta".to_string(),
            sensor_target_platform: self.target.as_str().to_string(),
            sensor_host_platform: self.host_platform.to_string(),
            sensor_name: self.name.clone(),
            sensor_beta: true,

            message: None,
            sensor_family: None,
            sensor_family_state: None,

            process_pid: None,
            process_parent_pid: None,
            process_name: None,
            process_cmd: None,
            process_exe: None,
            process_status: None,
            process_cpu_usage: None,
            process_memory_bytes: None,
            process_virtual_memory_bytes: None,
            process_start_time_unix_sec: None,
            process_disk_read_delta_bytes: None,
            process_disk_write_delta_bytes: None,

            network_interface: None,
            network_received_delta_bytes: None,
            network_transmitted_delta_bytes: None,
            network_received_total_bytes: None,
            network_transmitted_total_bytes: None,
            network_packets_received_delta: None,
            network_packets_transmitted_delta: None,
            network_errors_received_delta: None,
            network_errors_transmitted_delta: None,

            disk_io_read_delta_bytes: None,
            disk_io_write_delta_bytes: None,
            disk_io_read_total_bytes: None,
            disk_io_write_total_bytes: None,
        }
    }

    fn control_row(&self, sensor_event: &str, message: &str) -> SensorRow {
        let mut row = self.base_row("sensor_control", sensor_event);
        row.message = Some(message.to_string());
        row
    }

    fn run_collection_cycle(&mut self, out: &mut Vec<SensorRow>) -> usize {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::everything(),
        );
        self.networks.refresh(true);

        let mut emitted = 0usize;
        let mut remaining = self.cfg.max_rows_per_poll;

        for family in &self.cfg.families {
            if remaining == 0 {
                break;
            }
            let count = match family {
                PlatformSensorFamily::Process => self.emit_process_rows(out, remaining),
                PlatformSensorFamily::Network => self.emit_network_rows(out, remaining),
                PlatformSensorFamily::DiskIo => self.emit_disk_io_rows(out, remaining),
            };
            emitted += count;
            remaining = remaining.saturating_sub(count);
        }

        emitted
    }

    fn emit_process_rows(&self, out: &mut Vec<SensorRow>, limit: usize) -> usize {
        let mut rows: Vec<(&sysinfo::Pid, &Process)> = self.system.processes().iter().collect();
        rows.sort_unstable_by_key(|(pid, _)| pid.as_u32());

        let mut emitted = 0usize;
        for (pid, process) in rows.into_iter().take(limit) {
            let disk_usage = process.disk_usage();
            let mut row = self.base_row("process", "snapshot");
            row.process_pid = Some(pid.as_u32());
            row.process_parent_pid = process.parent().map(sysinfo::Pid::as_u32);
            row.process_name = Some(os_to_string(process.name()));
            row.process_cmd = Some(os_vec_to_string(process.cmd()));
            row.process_exe = process.exe().map(|p| p.to_string_lossy().to_string());
            row.process_status = Some(format!("{:?}", process.status()));
            row.process_cpu_usage = Some(process.cpu_usage());
            row.process_memory_bytes = Some(process.memory());
            row.process_virtual_memory_bytes = Some(process.virtual_memory());
            row.process_start_time_unix_sec = Some(process.start_time());
            row.process_disk_read_delta_bytes = Some(disk_usage.read_bytes);
            row.process_disk_write_delta_bytes = Some(disk_usage.written_bytes);
            out.push(row);
            emitted += 1;
        }

        emitted
    }

    fn emit_network_rows(&self, out: &mut Vec<SensorRow>, limit: usize) -> usize {
        let mut rows: Vec<_> = self.networks.iter().collect();
        rows.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

        let mut emitted = 0usize;
        for (iface, data) in rows.into_iter().take(limit) {
            let mut row = self.base_row("network", "snapshot");
            row.network_interface = Some(iface.to_string());
            row.network_received_delta_bytes = Some(data.received());
            row.network_transmitted_delta_bytes = Some(data.transmitted());
            row.network_received_total_bytes = Some(data.total_received());
            row.network_transmitted_total_bytes = Some(data.total_transmitted());
            row.network_packets_received_delta = Some(data.packets_received());
            row.network_packets_transmitted_delta = Some(data.packets_transmitted());
            row.network_errors_received_delta = Some(data.errors_on_received());
            row.network_errors_transmitted_delta = Some(data.errors_on_transmitted());
            out.push(row);
            emitted += 1;
        }

        emitted
    }

    fn emit_disk_io_rows(&self, out: &mut Vec<SensorRow>, limit: usize) -> usize {
        let mut rows: Vec<(&sysinfo::Pid, &Process)> = self
            .system
            .processes()
            .iter()
            .filter(|(_, process)| {
                let usage = process.disk_usage();
                usage.read_bytes > 0 || usage.written_bytes > 0
            })
            .collect();
        rows.sort_unstable_by(|(a_pid, a_proc), (b_pid, b_proc)| {
            let a = a_proc.disk_usage().written_bytes + a_proc.disk_usage().read_bytes;
            let b = b_proc.disk_usage().written_bytes + b_proc.disk_usage().read_bytes;
            b.cmp(&a).then_with(|| a_pid.as_u32().cmp(&b_pid.as_u32()))
        });

        let mut emitted = 0usize;
        for (pid, process) in rows.into_iter().take(limit) {
            let usage = process.disk_usage();
            let mut row = self.base_row("disk_io", "snapshot");
            row.process_pid = Some(pid.as_u32());
            row.process_name = Some(os_to_string(process.name()));
            row.disk_io_read_delta_bytes = Some(usage.read_bytes);
            row.disk_io_write_delta_bytes = Some(usage.written_bytes);
            row.disk_io_read_total_bytes = Some(usage.total_read_bytes);
            row.disk_io_write_total_bytes = Some(usage.total_written_bytes);
            out.push(row);
            emitted += 1;
        }

        emitted
    }

    fn emit_startup_control(&self, out: &mut Vec<SensorRow>) {
        out.push(self.control_row("startup", "platform sensor beta startup"));

        for family in &self.cfg.families {
            let mut row = self.base_row("sensor_control", "capability");
            row.sensor_family = Some(family.as_str().to_string());
            row.sensor_family_state = Some("beta_supported".to_string());
            out.push(row);
        }
    }
}

impl PlatformSensorMachine<InitState> {
    fn start(self, out: &mut Vec<SensorRow>) -> PlatformSensorMachine<RunningState> {
        self.common.emit_startup_control(out);
        PlatformSensorMachine {
            common: self.common,
            state: RunningState {
                last_cycle: Instant::now(),
            },
        }
    }
}

impl PlatformSensorMachine<RunningState> {
    fn poll_collection_if_due(&mut self, out: &mut Vec<SensorRow>, force: bool) {
        if force || self.state.last_cycle.elapsed() >= self.common.cfg.poll_interval {
            let emitted_rows = self.common.run_collection_cycle(out);
            if emitted_rows == 0 && self.common.cfg.emit_heartbeat {
                out.push(
                    self.common
                        .control_row("heartbeat", "platform sensor beta heartbeat"),
                );
            }
            self.state.last_cycle = Instant::now();
        }
    }
}

impl InputSource for PlatformSensorBetaInput {
    fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        let mut rows = Vec::with_capacity(self.machine_capacity_hint());
        let machine = self
            .machine
            .take()
            .ok_or_else(|| io::Error::other("platform sensor beta state missing"))?;

        let next_machine = match machine {
            PlatformSensorMachineState::Init(init) => {
                let mut running = init.start(&mut rows);
                running.poll_collection_if_due(&mut rows, true);
                PlatformSensorMachineState::Running(running)
            }
            PlatformSensorMachineState::Running(mut running) => {
                running.poll_collection_if_due(&mut rows, false);
                PlatformSensorMachineState::Running(running)
            }
        };
        self.machine = Some(next_machine);

        if rows.is_empty() {
            return Ok(vec![]);
        }

        let batch = sensor_rows_to_batch(rows)?;
        Ok(vec![InputEvent::Batch {
            batch,
            source_id: None,
            accounted_bytes: 0,
        }])
    }

    fn name(&self) -> &str {
        match self
            .machine
            .as_ref()
            .expect("platform sensor beta state should always be present")
        {
            PlatformSensorMachineState::Init(init) => &init.common.name,
            PlatformSensorMachineState::Running(running) => &running.common.name,
        }
    }

    fn health(&self) -> ComponentHealth {
        ComponentHealth::Healthy
    }
}

impl PlatformSensorBetaInput {
    fn machine_capacity_hint(&self) -> usize {
        match self
            .machine
            .as_ref()
            .expect("platform sensor beta state should always be present")
        {
            PlatformSensorMachineState::Init(init) => {
                init.common.cfg.max_rows_per_poll + init.common.cfg.families.len() + 2
            }
            PlatformSensorMachineState::Running(running) => {
                running.common.cfg.max_rows_per_poll + 2
            }
        }
    }
}

fn sensor_rows_to_batch(rows: Vec<SensorRow>) -> io::Result<RecordBatch> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp_unix_nano", DataType::UInt64, false),
        Field::new("level", DataType::Utf8, false),
        Field::new("event_family", DataType::Utf8, false),
        Field::new("sensor_event", DataType::Utf8, false),
        Field::new("sensor_status", DataType::Utf8, false),
        Field::new("sensor_target_platform", DataType::Utf8, false),
        Field::new("sensor_host_platform", DataType::Utf8, false),
        Field::new("sensor_name", DataType::Utf8, false),
        Field::new("sensor_beta", DataType::Boolean, false),
        Field::new("message", DataType::Utf8, true),
        Field::new("sensor_family", DataType::Utf8, true),
        Field::new("sensor_family_state", DataType::Utf8, true),
        Field::new("process_pid", DataType::UInt32, true),
        Field::new("process_parent_pid", DataType::UInt32, true),
        Field::new("process_name", DataType::Utf8, true),
        Field::new("process_cmd", DataType::Utf8, true),
        Field::new("process_exe", DataType::Utf8, true),
        Field::new("process_status", DataType::Utf8, true),
        Field::new("process_cpu_usage", DataType::Float32, true),
        Field::new("process_memory_bytes", DataType::UInt64, true),
        Field::new("process_virtual_memory_bytes", DataType::UInt64, true),
        Field::new("process_start_time_unix_sec", DataType::UInt64, true),
        Field::new("process_disk_read_delta_bytes", DataType::UInt64, true),
        Field::new("process_disk_write_delta_bytes", DataType::UInt64, true),
        Field::new("network_interface", DataType::Utf8, true),
        Field::new("network_received_delta_bytes", DataType::UInt64, true),
        Field::new("network_transmitted_delta_bytes", DataType::UInt64, true),
        Field::new("network_received_total_bytes", DataType::UInt64, true),
        Field::new("network_transmitted_total_bytes", DataType::UInt64, true),
        Field::new("network_packets_received_delta", DataType::UInt64, true),
        Field::new("network_packets_transmitted_delta", DataType::UInt64, true),
        Field::new("network_errors_received_delta", DataType::UInt64, true),
        Field::new("network_errors_transmitted_delta", DataType::UInt64, true),
        Field::new("disk_io_read_delta_bytes", DataType::UInt64, true),
        Field::new("disk_io_write_delta_bytes", DataType::UInt64, true),
        Field::new("disk_io_read_total_bytes", DataType::UInt64, true),
        Field::new("disk_io_write_total_bytes", DataType::UInt64, true),
    ]));

    let arrays: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.timestamp_unix_nano)
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter().map(|r| r.level.as_str()).collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.event_family.as_str())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.sensor_event.as_str())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.sensor_status.as_str())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.sensor_target_platform.as_str())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.sensor_host_platform.as_str())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.sensor_name.as_str())
                .collect::<Vec<_>>(),
        )),
        Arc::new(BooleanArray::from(
            rows.iter().map(|r| r.sensor_beta).collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.message.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.sensor_family.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.sensor_family_state.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt32Array::from(
            rows.iter().map(|r| r.process_pid).collect::<Vec<_>>(),
        )),
        Arc::new(UInt32Array::from(
            rows.iter()
                .map(|r| r.process_parent_pid)
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.process_name.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.process_cmd.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.process_exe.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.process_status.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(Float32Array::from(
            rows.iter().map(|r| r.process_cpu_usage).collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.process_memory_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.process_virtual_memory_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.process_start_time_unix_sec)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.process_disk_read_delta_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.process_disk_write_delta_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.network_interface.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.network_received_delta_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.network_transmitted_delta_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.network_received_total_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.network_transmitted_total_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.network_packets_received_delta)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.network_packets_transmitted_delta)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.network_errors_received_delta)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.network_errors_transmitted_delta)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.disk_io_read_delta_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.disk_io_write_delta_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.disk_io_read_total_bytes)
                .collect::<Vec<_>>(),
        )),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|r| r.disk_io_write_total_bytes)
                .collect::<Vec<_>>(),
        )),
    ];

    RecordBatch::try_new(schema, arrays)
        .map_err(|e| io::Error::other(format!("platform sensor beta batch build failed: {e}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPlatform {
    Linux,
    Macos,
    Windows,
    Unsupported,
}

impl HostPlatform {
    const fn as_str(self) -> Option<&'static str> {
        match self {
            Self::Linux => Some("linux"),
            Self::Macos => Some("macos"),
            Self::Windows => Some("windows"),
            Self::Unsupported => None,
        }
    }
}

fn os_to_string(value: &OsStr) -> String {
    value.to_string_lossy().to_string()
}

fn os_vec_to_string(values: &[std::ffi::OsString]) -> String {
    values
        .iter()
        .map(|v| v.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_unix_nano() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn current_host_platform() -> HostPlatform {
    if cfg!(target_os = "linux") {
        HostPlatform::Linux
    } else if cfg!(target_os = "macos") {
        HostPlatform::Macos
    } else if cfg!(target_os = "windows") {
        HostPlatform::Windows
    } else {
        HostPlatform::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, StringArray};

    fn host_target() -> PlatformSensorTarget {
        #[cfg(target_os = "linux")]
        {
            PlatformSensorTarget::Linux
        }
        #[cfg(target_os = "macos")]
        {
            PlatformSensorTarget::Macos
        }
        #[cfg(target_os = "windows")]
        {
            PlatformSensorTarget::Windows
        }
    }

    fn non_host_target() -> PlatformSensorTarget {
        #[cfg(target_os = "linux")]
        {
            PlatformSensorTarget::Macos
        }
        #[cfg(target_os = "macos")]
        {
            PlatformSensorTarget::Windows
        }
        #[cfg(target_os = "windows")]
        {
            PlatformSensorTarget::Linux
        }
    }

    fn string_col<'a>(batch: &'a RecordBatch, name: &str) -> &'a StringArray {
        batch
            .column_by_name(name)
            .unwrap_or_else(|| panic!("missing column: {name}"))
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap_or_else(|| panic!("column is not StringArray: {name}"))
    }

    fn contains_value(array: &StringArray, value: &str) -> bool {
        (0..array.len()).any(|i| !array.is_null(i) && array.value(i) == value)
    }

    #[test]
    fn rejects_non_matching_platform_target() {
        let err = PlatformSensorBetaInput::new(
            "beta",
            non_host_target(),
            PlatformSensorBetaConfig::default(),
        )
        .expect_err("non-matching target must fail");
        assert!(err.to_string().contains("can only run on"));
    }

    #[test]
    fn emits_startup_and_snapshot_rows_on_first_poll() {
        let mut input = PlatformSensorBetaInput::new(
            "beta",
            host_target(),
            PlatformSensorBetaConfig {
                emit_heartbeat: false,
                poll_interval: Duration::from_secs(3600),
                families: vec![PlatformSensorFamily::Process],
                max_rows_per_poll: 1,
            },
        )
        .expect("host target should be valid");

        let events = input.poll().expect("poll should succeed");
        assert_eq!(events.len(), 1);
        let batch = match &events[0] {
            InputEvent::Batch { batch, .. } => batch,
            _ => panic!("expected Batch event"),
        };

        let sensor_event = string_col(batch, "sensor_event");
        assert!(contains_value(sensor_event, "startup"));
        assert!(contains_value(sensor_event, "capability"));

        let sensor_family = string_col(batch, "sensor_family");
        assert!(contains_value(sensor_family, "process"));
    }

    #[test]
    fn heartbeat_disabled_emits_only_first_cycle_until_interval_elapses() {
        let mut input = PlatformSensorBetaInput::new(
            "beta",
            host_target(),
            PlatformSensorBetaConfig {
                emit_heartbeat: false,
                poll_interval: Duration::from_secs(3600),
                families: vec![PlatformSensorFamily::DiskIo],
                max_rows_per_poll: 1,
            },
        )
        .expect("host target should be valid");

        assert_eq!(input.poll().expect("startup poll").len(), 1);
        assert!(input.poll().expect("second poll").is_empty());
    }
}
