//! MVP fault scenario harness for turmoil simulations.
//!
//! The harness composes fault injection inputs (sink behavior scripts,
//! checkpoint flush crashes, and turmoil network events) as data and runs
//! invariant checks against a structured `TestOutcome`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use logfwd::pipeline::Pipeline;
use logfwd_test_utils::sinks::CountingSink;
use logfwd_types::pipeline::SourceId;
use tokio_util::sync::CancellationToken;

use super::channel_input::ChannelInputSource;
use super::instrumented_sink::{FailureAction, InstrumentedSink};
use super::observable_checkpoint::{CheckpointHandle, ObservableCheckpointStore};
use super::tcp_server::{TcpServerHandle, run_tcp_server};
use super::turmoil_tcp_sink::TurmoilTcpSink;

const DEFAULT_SIM_DURATION_SECS: u64 = 60;
const DEFAULT_TICK_MS: u64 = 1;
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 20;
const DEFAULT_TCP_PORT: u16 = 9137;

#[derive(Clone, Debug)]
pub enum NetworkFaultAction {
    Partition,
    Repair,
}

#[derive(Clone, Debug)]
pub struct NetworkFault {
    step: usize,
    action: NetworkFaultAction,
}

impl NetworkFault {
    pub fn at_step(step: usize, action: NetworkFaultAction) -> Self {
        Self { step, action }
    }
}

#[derive(Clone, Debug)]
pub enum SinkMode {
    Instrumented { script: Vec<FailureAction> },
    TurmoilTcp,
    Counting,
}

#[derive(Clone, Debug)]
pub struct FaultScenario {
    name: String,
    seed: u64,
    source_id: SourceId,
    lines: usize,
    sink_mode: SinkMode,
    duration_secs: u64,
    tick_ms: u64,
    batch_timeout_ms: u64,
    shutdown_after: Duration,
    checkpoint_flush_interval: Option<Duration>,
    arm_checkpoint_crash_after: Option<Duration>,
    network_faults: Vec<NetworkFault>,
    fail_rate: Option<f64>,
}

impl FaultScenario {
    pub fn builder(name: &str) -> Self {
        Self {
            name: name.to_string(),
            seed: super::turmoil_seed(),
            source_id: SourceId(1),
            lines: 10,
            sink_mode: SinkMode::Counting,
            duration_secs: DEFAULT_SIM_DURATION_SECS,
            tick_ms: DEFAULT_TICK_MS,
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
            shutdown_after: Duration::from_secs(5),
            checkpoint_flush_interval: None,
            arm_checkpoint_crash_after: None,
            network_faults: Vec::new(),
            fail_rate: None,
        }
    }

    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    pub fn with_line_count(mut self, lines: usize) -> Self {
        self.lines = lines;
        self
    }

    pub fn with_sink_script(mut self, script: Vec<FailureAction>) -> Self {
        self.sink_mode = SinkMode::Instrumented { script };
        self
    }

    pub fn with_turmoil_tcp_sink(mut self) -> Self {
        self.sink_mode = SinkMode::TurmoilTcp;
        self
    }

    pub fn with_counting_sink(mut self) -> Self {
        self.sink_mode = SinkMode::Counting;
        self
    }

    pub fn with_shutdown_after(mut self, shutdown_after: Duration) -> Self {
        self.shutdown_after = shutdown_after;
        self
    }

    pub fn with_batch_timeout(mut self, timeout: Duration) -> Self {
        self.batch_timeout_ms = timeout.as_millis() as u64;
        self
    }

    pub fn with_checkpoint_flush_interval(mut self, interval: Duration) -> Self {
        self.checkpoint_flush_interval = Some(interval);
        self
    }

    pub fn with_checkpoint_crash_after(mut self, crash_after: Duration) -> Self {
        self.arm_checkpoint_crash_after = Some(crash_after);
        self
    }

    pub fn with_network_fault(mut self, fault: NetworkFault) -> Self {
        self.network_faults.push(fault);
        self
    }

    pub fn with_fail_rate(mut self, fail_rate: f64) -> Self {
        self.fail_rate = Some(fail_rate);
        self
    }

    pub fn run(self) -> TestOutcome {
        let scenario_name = self.name.clone();
        let seed = self.seed;
        let mut builder = turmoil::Builder::new();
        builder
            .rng_seed(seed)
            .enable_random_order()
            .simulation_duration(Duration::from_secs(self.duration_secs))
            .tick_duration(Duration::from_millis(self.tick_ms));
        if let Some(fail_rate) = self.fail_rate {
            builder.fail_rate(fail_rate);
        }
        let mut sim = builder.build();

        let mut delivered_counter = Arc::new(AtomicU64::new(0));
        let mut call_counter = Arc::new(AtomicU64::new(0));
        let mut checkpoint_handle: Option<CheckpointHandle> = None;
        let mut tcp_server_handle: Option<TcpServerHandle> = None;

        match &self.sink_mode {
            SinkMode::TurmoilTcp => {
                let server = TcpServerHandle::new();
                let sh = server.clone();
                sim.host("server", move || {
                    let host_handle = sh.clone();
                    async move {
                        run_tcp_server(DEFAULT_TCP_PORT, host_handle).await?;
                        Ok(())
                    }
                });
                delivered_counter = server.received_lines.clone();
                tcp_server_handle = Some(server);

                let scenario = self.clone();
                sim.client("pipeline", async move {
                    let lines = generate_json_lines(scenario.lines);
                    let input = ChannelInputSource::new("scenario", scenario.source_id, lines);

                    let sink = TurmoilTcpSink::new("server", DEFAULT_TCP_PORT);
                    let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
                    pipeline.set_batch_timeout(Duration::from_millis(scenario.batch_timeout_ms));
                    if let Some(interval) = scenario.checkpoint_flush_interval {
                        pipeline.set_checkpoint_flush_interval(interval);
                    }
                    let mut pipeline = pipeline.with_input("scenario", Box::new(input));

                    let shutdown = CancellationToken::new();
                    let sd = shutdown.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(scenario.shutdown_after).await;
                        sd.cancel();
                    });

                    pipeline.run_async(&shutdown).await?;
                    Ok(())
                });
            }
            SinkMode::Instrumented { script } => {
                let sink = InstrumentedSink::new(script.clone());
                delivered_counter = sink.delivered_counter();
                call_counter = sink.call_counter();

                let scenario = self.clone();
                let maybe_checkpoint = if scenario.checkpoint_flush_interval.is_some()
                    || scenario.arm_checkpoint_crash_after.is_some()
                {
                    let (store, handle) = ObservableCheckpointStore::new();
                    checkpoint_handle = Some(handle.clone());
                    Some((store, handle))
                } else {
                    None
                };

                sim.client("pipeline", async move {
                    let lines = generate_json_lines(scenario.lines);
                    let input = ChannelInputSource::new("scenario", scenario.source_id, lines);
                    let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
                    pipeline.set_batch_timeout(Duration::from_millis(scenario.batch_timeout_ms));

                    let mut pipeline = pipeline.with_input("scenario", Box::new(input));

                    if let Some(interval) = scenario.checkpoint_flush_interval {
                        pipeline.set_checkpoint_flush_interval(interval);
                    }

                    if let Some((store, handle)) = maybe_checkpoint {
                        pipeline = pipeline.with_checkpoint_store(Box::new(store));
                        if let Some(crash_after) = scenario.arm_checkpoint_crash_after {
                            tokio::spawn(async move {
                                tokio::time::sleep(crash_after).await;
                                handle.arm_crash();
                            });
                        }
                    }

                    let shutdown = CancellationToken::new();
                    let sd = shutdown.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(scenario.shutdown_after).await;
                        sd.cancel();
                    });

                    pipeline.run_async(&shutdown).await?;
                    Ok(())
                });
            }
            SinkMode::Counting => {
                let sink = CountingSink::new(delivered_counter.clone());
                let scenario = self.clone();

                let maybe_checkpoint = if scenario.checkpoint_flush_interval.is_some()
                    || scenario.arm_checkpoint_crash_after.is_some()
                {
                    let (store, handle) = ObservableCheckpointStore::new();
                    checkpoint_handle = Some(handle.clone());
                    Some((store, handle))
                } else {
                    None
                };

                sim.client("pipeline", async move {
                    let lines = generate_json_lines(scenario.lines);
                    let input = ChannelInputSource::new("scenario", scenario.source_id, lines);
                    let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
                    pipeline.set_batch_timeout(Duration::from_millis(scenario.batch_timeout_ms));

                    let mut pipeline = pipeline.with_input("scenario", Box::new(input));

                    if let Some(interval) = scenario.checkpoint_flush_interval {
                        pipeline.set_checkpoint_flush_interval(interval);
                    }

                    if let Some((store, handle)) = maybe_checkpoint {
                        pipeline = pipeline.with_checkpoint_store(Box::new(store));
                        if let Some(crash_after) = scenario.arm_checkpoint_crash_after {
                            tokio::spawn(async move {
                                tokio::time::sleep(crash_after).await;
                                handle.arm_crash();
                            });
                        }
                    }

                    let shutdown = CancellationToken::new();
                    let sd = shutdown.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(scenario.shutdown_after).await;
                        sd.cancel();
                    });

                    pipeline.run_async(&shutdown).await?;
                    Ok(())
                });
            }
        }

        let mut network_faults = self.network_faults.clone();
        network_faults.sort_by_key(|f| f.step);

        let run_outcome = catch_unwind(AssertUnwindSafe(|| {
            if network_faults.is_empty() {
                sim.run()
            } else {
                let mut faults = network_faults.iter().peekable();
                let mut step = 0usize;
                while faults.peek().is_some() {
                    sim.step()?;
                    step += 1;
                    while let Some(fault) = faults.peek() {
                        if fault.step != step {
                            break;
                        }
                        apply_network_fault(&mut sim, fault);
                        faults.next();
                    }
                }
                sim.run()
            }
        }));

        let (panicked, sim_error) = match run_outcome {
            Ok(Ok(())) => (false, None),
            Ok(Err(err)) => (false, Some(err.to_string())),
            Err(_) => (true, None),
        };

        TestOutcome {
            scenario_name,
            seed,
            delivered_rows: delivered_counter.load(Ordering::Relaxed),
            send_calls: call_counter.load(Ordering::Relaxed),
            panicked,
            sim_error,
            checkpoint: checkpoint_handle,
            tcp_server: tcp_server_handle,
        }
    }
}

fn apply_network_fault(sim: &mut turmoil::Sim<'_>, fault: &NetworkFault) {
    match fault.action {
        NetworkFaultAction::Partition => sim.partition("pipeline", "server"),
        NetworkFaultAction::Repair => sim.repair("pipeline", "server"),
    }
}

fn generate_json_lines(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("{{\"msg\":\"line {i}\",\"num\":{i}}}\n").into_bytes())
        .collect()
}

pub struct TestOutcome {
    scenario_name: String,
    seed: u64,
    delivered_rows: u64,
    send_calls: u64,
    panicked: bool,
    sim_error: Option<String>,
    checkpoint: Option<CheckpointHandle>,
    tcp_server: Option<TcpServerHandle>,
}

impl TestOutcome {
    pub fn delivered_rows(&self) -> u64 {
        self.delivered_rows
    }

    pub fn send_calls(&self) -> u64 {
        self.send_calls
    }

    pub fn panicked(&self) -> bool {
        self.panicked
    }

    pub fn sim_error(&self) -> Option<&str> {
        self.sim_error.as_deref()
    }

    pub fn checkpoint(&self) -> Option<&CheckpointHandle> {
        self.checkpoint.as_ref()
    }

    pub fn server_received(&self) -> Option<u64> {
        self.tcp_server
            .as_ref()
            .map(|h| h.received_lines.load(Ordering::Relaxed))
    }

    pub fn server_connections(&self) -> Option<u64> {
        self.tcp_server
            .as_ref()
            .map(|h| h.connection_count.load(Ordering::Relaxed))
    }

    pub fn replay_hint(&self) -> String {
        format!(
            "replay with TURMOIL_SEED={} cargo test -p logfwd --features turmoil --test turmoil_sim",
            self.seed
        )
    }
}

#[derive(Clone, Debug)]
enum Invariant {
    NoSimError,
    DeliveredEq(u64),
    CallsGe(u64),
    CheckpointMonotonic { source_id: u64 },
    CheckpointCrashCountGe(u64),
    CheckpointUpdatesGe { source_id: u64, min: usize },
    ServerReceivedGe(u64),
    ServerConnectionsGe(u64),
}

#[derive(Clone, Debug, Default)]
pub struct InvariantSet {
    invariants: Vec<Invariant>,
}

impl InvariantSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn no_sim_error(mut self) -> Self {
        self.invariants.push(Invariant::NoSimError);
        self
    }

    pub fn delivered_eq(mut self, expected: u64) -> Self {
        self.invariants.push(Invariant::DeliveredEq(expected));
        self
    }

    pub fn calls_ge(mut self, minimum: u64) -> Self {
        self.invariants.push(Invariant::CallsGe(minimum));
        self
    }

    pub fn checkpoint_monotonic(mut self, source_id: u64) -> Self {
        self.invariants
            .push(Invariant::CheckpointMonotonic { source_id });
        self
    }

    pub fn checkpoint_crash_count_ge(mut self, min: u64) -> Self {
        self.invariants.push(Invariant::CheckpointCrashCountGe(min));
        self
    }

    pub fn checkpoint_updates_ge(mut self, source_id: u64, min: usize) -> Self {
        self.invariants
            .push(Invariant::CheckpointUpdatesGe { source_id, min });
        self
    }

    pub fn server_received_ge(mut self, minimum: u64) -> Self {
        self.invariants.push(Invariant::ServerReceivedGe(minimum));
        self
    }

    pub fn server_connections_ge(mut self, minimum: u64) -> Self {
        self.invariants
            .push(Invariant::ServerConnectionsGe(minimum));
        self
    }

    pub fn verify(&self, outcome: &TestOutcome) {
        for invariant in &self.invariants {
            match invariant {
                Invariant::NoSimError => {
                    assert!(
                        outcome.sim_error().is_none(),
                        "scenario '{}' failed: sim error {:?} ({})",
                        outcome.scenario_name,
                        outcome.sim_error(),
                        outcome.replay_hint()
                    );
                    assert!(
                        !outcome.panicked(),
                        "scenario '{}' failed: panic observed ({})",
                        outcome.scenario_name,
                        outcome.replay_hint()
                    );
                }
                Invariant::DeliveredEq(expected) => {
                    assert_eq!(
                        outcome.delivered_rows(),
                        *expected,
                        "scenario '{}' delivered_rows mismatch ({})",
                        outcome.scenario_name,
                        outcome.replay_hint()
                    );
                }
                Invariant::CallsGe(minimum) => {
                    assert!(
                        outcome.send_calls() >= *minimum,
                        "scenario '{}' expected send_calls >= {}, got {} ({})",
                        outcome.scenario_name,
                        minimum,
                        outcome.send_calls(),
                        outcome.replay_hint()
                    );
                }
                Invariant::CheckpointMonotonic { source_id } => {
                    let checkpoint = outcome
                        .checkpoint()
                        .expect("checkpoint invariant requested without checkpoint handle");
                    checkpoint.assert_monotonic(*source_id);
                }
                Invariant::CheckpointCrashCountGe(minimum) => {
                    let checkpoint = outcome
                        .checkpoint()
                        .expect("checkpoint invariant requested without checkpoint handle");
                    assert!(
                        checkpoint.crash_count() >= *minimum,
                        "scenario '{}' expected crash_count >= {}, got {} ({})",
                        outcome.scenario_name,
                        minimum,
                        checkpoint.crash_count(),
                        outcome.replay_hint()
                    );
                }
                Invariant::CheckpointUpdatesGe { source_id, min } => {
                    let checkpoint = outcome
                        .checkpoint()
                        .expect("checkpoint invariant requested without checkpoint handle");
                    assert!(
                        checkpoint.update_count(*source_id) >= *min,
                        "scenario '{}' expected checkpoint updates for source {} >= {}, got {} ({})",
                        outcome.scenario_name,
                        source_id,
                        min,
                        checkpoint.update_count(*source_id),
                        outcome.replay_hint()
                    );
                }
                Invariant::ServerReceivedGe(minimum) => {
                    let received = outcome
                        .server_received()
                        .expect("server invariant requested without server handle");
                    assert!(
                        received >= *minimum,
                        "scenario '{}' expected server_received >= {}, got {} ({})",
                        outcome.scenario_name,
                        minimum,
                        received,
                        outcome.replay_hint()
                    );
                }
                Invariant::ServerConnectionsGe(minimum) => {
                    let connections = outcome
                        .server_connections()
                        .expect("server invariant requested without server handle");
                    assert!(
                        connections >= *minimum,
                        "scenario '{}' expected server_connections >= {}, got {} ({})",
                        outcome.scenario_name,
                        minimum,
                        connections,
                        outcome.replay_hint()
                    );
                }
            }
        }
    }
}
