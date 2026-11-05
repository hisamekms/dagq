//! The host as the environment the runs execute in (ADR-t1662-3 decisions
//! 3 to 6, [`ExecutionEnvironment`]): the node is the supervisor's host
//! ([`HOST_NODE`], as its NodeSampleStore rows), with no image, its cores
//! and its memory as the limits and its architecture. It measures a run
//! from outside by the processes the run registered and every process
//! under them, read from the OS itself ([`measure_processes`]), and the node
//! from the host's load's sample, with the CPU pressure of
//! `/proc/pressure/cpu` where the host has it (Linux). It adds no wait to a
//! run.

use std::{collections::BTreeMap, path::Path, sync::Mutex, time::Duration};

use anyhow::{Context, Result};

use super::host_metrics::tool;
use super::process_read::measure_processes;
use super::queue_service::unix_now;
use crate::application::ExecutionEnvironment;
use crate::domain::RunId;
use crate::domain::environment::{
    EnvKind, EnvironmentAttached, EnvironmentWait, Process, ProcessMeter, Resources, RunProcesses,
    cpu_pressure, host_node_resources,
};
use crate::domain::host_metrics::{HOST_NODE, HostSample, darwin_memory, linux_memory};

/// The host as an [`ExecutionEnvironment`].
pub struct HostEnvironment {
    cores: Option<f64>,
    memory_bytes: Option<i64>,
    /// The processes now, or why they did not list.
    processes: Box<dyn Fn() -> Result<Vec<Process>> + Send + Sync>,
    /// The CPU pressure now, where the host has it.
    pressure: Box<dyn Fn() -> Option<f64> + Send + Sync>,
    /// What each run's measure keeps between its windows.
    meters: Mutex<BTreeMap<String, ProcessMeter>>,
}

impl HostEnvironment {
    /// This host: its cores, its memory, its processes and its PSI.
    pub fn new() -> Self {
        #[allow(clippy::cast_precision_loss)]
        let cores = std::thread::available_parallelism()
            .ok()
            .map(|cores| cores.get() as f64);
        Self::with(
            cores,
            memory_bytes(),
            || measure_processes(unix_now()),
            pressure,
        )
    }

    /// A host of `cores` and `memory_bytes`, read through `processes`
    /// and `pressure`: the tests'.
    pub fn with(
        cores: Option<f64>,
        memory_bytes: Option<i64>,
        processes: impl Fn() -> Result<Vec<Process>> + Send + Sync + 'static,
        pressure: impl Fn() -> Option<f64> + Send + Sync + 'static,
    ) -> Self {
        Self {
            cores,
            memory_bytes,
            processes: Box::new(processes),
            pressure: Box::new(pressure),
            meters: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Default for HostEnvironment {
    fn default() -> Self {
        Self::new()
    }
}

/// The host's memory in bytes, the Linux way when `/proc/meminfo` exists,
/// else the macOS way.
fn memory_bytes() -> Option<i64> {
    let memory = if Path::new("/proc/meminfo").exists() {
        linux_memory(
            std::fs::read_to_string("/proc/meminfo").ok().as_deref(),
            None,
        )
    } else {
        darwin_memory(None, tool("sysctl", &["-n", "hw.memsize"]).as_deref(), None)
    };
    #[allow(clippy::cast_possible_truncation)]
    memory.total_mb.map(|mb| (mb * 1_048_576.0).round() as i64)
}

fn pressure() -> Option<f64> {
    cpu_pressure(&std::fs::read_to_string("/proc/pressure/cpu").ok()?)
}

impl ExecutionEnvironment for HostEnvironment {
    fn attached(&self) -> EnvironmentAttached {
        EnvironmentAttached {
            env_kind: EnvKind::Host,
            node_id: HOST_NODE.to_owned(),
            image_digest: None,
            cpu_limit: self.cores,
            mem_limit: self.memory_bytes,
            arch: std::env::consts::ARCH.to_owned(),
        }
    }

    fn measures_from_outside(&self) -> bool {
        true
    }

    fn waits(&self) -> &'static [EnvironmentWait] {
        &[]
    }

    fn measure_runs(&self, runs: &[RunProcesses]) -> Result<Vec<(RunId, Resources)>> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let listed = (self.processes)().context("the processes could not be listed")?;
        let mut meters = self
            .meters
            .lock()
            .map_err(|_| anyhow::anyhow!("the measures of the runs were poisoned"))?;
        let mut measured = Vec::new();
        for asked in runs {
            let run = &asked.run;
            let meter = meters.entry(run.to_string()).or_default();
            let (cpu_seconds, memory_max_bytes) =
                meter.window(&listed, &asked.pids, asked.window_secs);
            if cpu_seconds.is_none() && memory_max_bytes.is_none() {
                continue;
            }
            measured.push((
                run.clone(),
                Resources {
                    cpu_seconds,
                    cpu_limit: self.cores,
                    memory_max_bytes,
                    ..Resources::default()
                },
            ));
        }
        Ok(measured)
    }

    fn forget_run(&self, run: &RunId) {
        if let Ok(mut meters) = self.meters.lock() {
            meters.remove(run.as_str());
        }
    }

    fn node_resources(&self, sample: &HostSample, interval: Duration) -> Resources {
        host_node_resources(
            sample,
            interval.as_secs_f64(),
            self.cores,
            (self.pressure)(),
        )
    }
}

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
mod tests;
