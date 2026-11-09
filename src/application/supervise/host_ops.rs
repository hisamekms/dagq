//! host運用 of the supervisor (rule C3 of docs/design/architecture.md):
//! its state ([`HostOpsState`]) and the submodules that change it. The
//! state's fields are private to this module, so no other context's
//! submodule can change them; the loop calls the state's methods.

use super::*;
use crate::application::HostOpsQueue;

pub(super) mod broker_relay;
pub(super) mod cleanup;
pub(super) mod disk;
pub(super) mod handoff;
pub(super) mod host_metrics;
pub(super) mod inbox_nudge;
#[cfg(test)]
mod job_fakes;
pub(super) mod moving_sizes;
pub(super) mod queue_broker;
pub(super) mod queue_service;
pub(super) mod release;
pub(super) mod sccache;
pub(super) mod sweep;
pub(super) mod update;

/// host運用's state: the queue service, the automatic update and the
/// release update, the sccache server, the free disk space and the cleanup
/// of what ended runs left, the sweep of their sessions, the record of the
/// host's load and its limit. The supervisor's registration and handoff
/// are [`handoff::Registration`]. The claim and the landing read the
/// disk's reading ([`HostOpsState::free`], [`disk::DiskWatch`]), the
/// sccache server's look and the load.
pub(super) struct HostOpsState {
    /// Opens the connections of the context's jobs off the loop.
    queues: Arc<dyn QueueOpener<dyn HostOpsQueue + Send>>,
    /// Records the host's load; `None` records none (task 516).
    host_metrics_port: Option<HostMetricsPort>,
    /// The sample job of the host's load.
    host_metrics: host_metrics::HostMetricsWatch,
    /// Keeps the queue's service; `None` keeps none.
    queue_service_port: Option<QueueServicePort>,
    /// What the supervisor knows of the queue's service.
    queue_service: queue_service::QueueServiceWatch,
    /// Whether the queue service ran at the last look, or no service is
    /// kept (ADR-t1233-4 decision 2): new claims and jobs wait for it.
    service_up: bool,
    /// Keeps the queue's broker; `None` keeps none.
    queue_broker_port: Option<super::QueueBrokerPort>,
    /// What the supervisor knows of the queue's broker.
    queue_broker: super::queue_broker::QueueBrokerWatch,
    /// Keeps the runs' relays; `None` keeps none.
    broker_relay_port: Option<super::QueueRelayPort>,
    /// What the supervisor knows of the runs' relays.
    broker_relay: super::broker_relay::QueueRelayWatch,
    /// The automatic update's look at main (ADR-0045 decision 17).
    update: update::UpdateWatch,
    /// Looks for a new release; `None` looks for none.
    release_port: Option<ReleasePort>,
    /// The release look running and when the last one started.
    release: release::ReleaseWatch,
    /// Looks at and starts the host's sccache server (ADR-t1215-1).
    sccache_port: Option<SccachePort>,
    sccache: sccache::SccacheWatch,
    /// `[disk]`: the reserve and when the disk ask opens (task 377,
    /// ADR-t4473-1).
    disk_config: crate::domain::disk::DiskConfig,
    /// Reads the free bytes of the file system of a path.
    free_space: fn(&Path) -> Option<u64>,
    /// Reads the size of the file system of a path.
    total_space: fn(&Path) -> Option<u64>,
    /// Lists the directories of the Claude Code scratchpads (task 1100).
    scratchpad_roots: ScratchpadRoots,
    /// The disk between passes (task 377).
    disk: disk::DiskWatch,
    /// Measures a tree's bytes on disk, for the runs moving.
    measure_tree: moving_sizes::MeasureTree,
    /// The sizes of the runs moving, measured off the loop (ADR-t4542-1).
    moving_sizes: moving_sizes::MovingSizes,
    /// The free bytes of the queue's directory read this pass.
    free: Option<u64>,
    /// The cleanup of ended runs' worktrees off the loop (task 405).
    cleanup: cleanup::CleanupWatch,
    /// The sweep of the session wrappers ended runs left running.
    sweep: sweep::SweepWatch,
    /// `--max-load` (task 327).
    max_load: Option<f64>,
    /// The 1-minute load average, and the host's versions a claim records.
    load_average: fn() -> Option<f64>,
    host_versions: fn(&Path, Option<&Path>, Option<&Path>) -> HostVersions,
}

/// What a landing's retry of its verification reads the disk with
/// ([`HostOpsState::disk_reader`]).
#[derive(Debug, Clone)]
pub(super) struct DiskReader {
    pub(super) config: crate::domain::disk::DiskConfig,
    pub(super) free_space: fn(&Path) -> Option<u64>,
    pub(super) total_space: fn(&Path) -> Option<u64>,
    /// The runs moving besides the landings under way at the latest
    /// pass's reading, read when it is needed (a retry comes minutes after
    /// the landing started).
    pub(super) moving: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl HostOpsState {
    pub(super) fn new(ports: &Ports<'_>, settings: &LoopSettings) -> Self {
        Self {
            queues: ports.host_queues.clone(),
            host_metrics_port: ports.host_metrics.clone(),
            host_metrics: host_metrics::HostMetricsWatch::default(),
            queue_service_port: ports.queue_service.clone(),
            queue_service: queue_service::QueueServiceWatch::default(),
            service_up: true,
            queue_broker_port: ports.queue_broker.clone(),
            queue_broker: super::queue_broker::QueueBrokerWatch::default(),
            broker_relay_port: ports.broker_relay.clone(),
            broker_relay: super::broker_relay::QueueRelayWatch::default(),
            update: update::UpdateWatch::default(),
            release_port: ports.release.clone(),
            release: release::ReleaseWatch::default(),
            sccache_port: ports.sccache.clone(),
            sccache: sccache::SccacheWatch::default(),
            disk_config: settings.disk,
            free_space: ports.free_space,
            total_space: ports.total_space,
            scratchpad_roots: ports.scratchpad_roots.clone(),
            disk: disk::DiskWatch::default(),
            measure_tree: ports.measure_tree.clone(),
            moving_sizes: moving_sizes::MovingSizes::default(),
            free: None,
            cleanup: cleanup::CleanupWatch::counting(settings.cleanup_jobs.clone()),
            sweep: sweep::SweepWatch::default(),
            max_load: settings.max_load,
            load_average: ports.load_average,
            host_versions: ports.host_versions,
        }
    }

    /// A job of the context runs that a handoff waits for: the cleanup,
    /// the sample of the host's load and the release look.
    pub(super) const fn handoff_waits(&self) -> bool {
        self.cleanup.running() || self.host_metrics.running() || self.release.running()
    }

    /// Whether the queue service ran at the last look, or none is kept.
    pub(super) const fn service_up(&self) -> bool {
        self.service_up
    }

    /// The release look runs.
    pub(super) const fn release_running(&self) -> bool {
        self.release.running()
    }

    /// The cleanup of ended runs, which the claim, the resume, the triage
    /// and the landing read and defer to.
    pub(super) const fn cleanup(&self) -> &cleanup::CleanupWatch {
        &self.cleanup
    }

    /// Hold the next cleanup back for a resume or a landing that takes the
    /// worktrees it would clean ([`cleanup::CleanupWatch::defer`]).
    pub(super) const fn defer_cleanup(&mut self) {
        self.cleanup.defer();
    }

    /// Start the sweep of what ended runs left when it is due
    /// ([`sweep::SweepWatch::start`]).
    pub(super) fn sweep_due(&mut self, now: Instant, interval: Duration) -> bool {
        self.sweep.start(now, interval)
    }

    /// Stop the background wrappers of ended runs still running
    /// ([`sweep::SweepWatch::sweep_ended_sessions`]).
    pub(super) fn sweep_ended_sessions(
        &mut self,
        env: &mut PassEnv<'_>,
        held: &[RunId],
    ) -> Result<()> {
        self.sweep.sweep_ended_sessions(env, held)
    }

    /// Prune the registrations other supervisors of the queue left behind
    /// ([`sweep::prune_left_supervisors`]).
    pub(super) fn prune_left_supervisors(&self, env: &PassEnv<'_>) -> Result<()> {
        sweep::prune_left_supervisors(
            &*env.queue,
            &**env.processes,
            env.token,
            env.generators.clock.now(),
        )
    }

    /// Remove what ended observations left, and the old backups, past
    /// `[retention]` of `dagq.toml` as read now
    /// ([`crate::application::retention::remove_expired`]; one that cannot
    /// be read keeps the defaults). Each removal is logged, a failure only
    /// warned of and retried on the next sweep.
    pub(super) fn remove_expired(&self, env: &PassEnv<'_>) {
        use crate::application::retention::{RetentionDirs, remove_expired};
        let config = env.verifier.retention_config().unwrap_or_else(|error| {
            warn!(error = %format_args!("{error:#}"), "[retention] of dagq.toml not read: {error:#}; using the defaults");
            Default::default()
        });
        let swept = remove_expired(
            &*env.queue,
            &**env.processes,
            &**env.files,
            &*env.generators.clock,
            &RetentionDirs::of(&env.layout.db, &**env.files),
            &config,
        );
        // What went is logged even when something else failed.
        for path in swept.removed {
            info!(path = %path.display(), retention_days = config.days, backups = config.backups, "removed {} past its retention", path.display());
        }
        if let Some(error) = swept.failure {
            warn!(error = %format_args!("{error:#}"), "what ended observations left, or the old backups, could not all be removed: {error:#}");
        }
    }

    /// Sample the host's load ([`host_metrics::HostMetricsWatch::pass`]).
    pub(super) fn host_metrics_pass(
        &mut self,
        handing_off: bool,
        now: impl FnOnce() -> i64,
        at: Instant,
        held: impl FnOnce() -> Option<host_metrics::HeldWorktrees>,
    ) {
        self.host_metrics
            .pass(self.host_metrics_port.as_ref(), handing_off, now, at, held);
    }

    /// Join or leave the sample of the host's load at the end of the loop
    /// ([`host_metrics::HostMetricsWatch::finish`]).
    pub(super) fn host_metrics_finish(
        &mut self,
        deadline: &EndBy,
        clock: &dyn crate::application::Clock,
    ) {
        self.host_metrics.finish(deadline, clock);
    }

    /// Join or leave the walk of the runs moving at the end of the loop
    /// ([`moving_sizes::MovingSizes::finish`]).
    pub(super) fn moving_sizes_finish(
        &mut self,
        deadline: &EndBy,
        clock: &dyn crate::application::Clock,
    ) {
        self.moving_sizes.finish(deadline, clock);
    }

    /// Tell the automatic update which heavy job held its check
    /// ([`update::UpdateWatch::tell_held_check`]).
    pub(super) fn tell_held_check(
        &mut self,
        check: &str,
        what: Option<crate::domain::heavy_turn::HeavyJob>,
    ) {
        self.update.tell_held_check(check, what);
    }

    /// `--max-load`.
    pub(super) const fn max_load(&self) -> Option<f64> {
        self.max_load
    }

    /// Reads the 1-minute load average.
    pub(super) const fn load_average(&self) -> fn() -> Option<f64> {
        self.load_average
    }

    /// Reads the host's versions a claim records.
    pub(super) const fn host_versions(
        &self,
    ) -> fn(&Path, Option<&Path>, Option<&Path>) -> HostVersions {
        self.host_versions
    }

    /// The free bytes of the queue's directory read this pass.
    pub(super) const fn free(&self) -> Option<u64> {
        self.free
    }

    /// The disk is short and a cleanup for room runs: claims wait for it.
    pub(super) const fn disk_cleaning(&self) -> bool {
        self.disk.cleaning
    }

    /// The disk is short of what a landing needs.
    pub(super) const fn landing_short(&self) -> bool {
        self.disk.landing_short
    }

    /// Install the first successfully read deployment snapshot before claims.
    pub(super) fn set_repository_disk(&mut self, config: crate::domain::disk::DiskConfig) {
        self.disk_config = config;
    }
    /// `[disk]`, the readers of the free and total bytes and the runs
    /// moving at this pass's reading, for a landing's retry of verification.
    pub(super) fn disk_reader(&self) -> DiskReader {
        DiskReader {
            config: self.disk_config,
            free_space: self.free_space,
            total_space: self.total_space,
            moving: self.disk.moving.clone(),
        }
    }

    /// What a claim and a landing need, as this pass read the disk.
    pub(super) const fn disk_needs(&self) -> crate::domain::disk::DiskNeeds {
        self.disk.needs
    }

    /// How many new runs this pass may claim for the disk; `None` for no
    /// limit.
    pub(super) const fn disk_claims(&self) -> Option<usize> {
        self.disk.claims
    }

    /// The port of the host's sccache server.
    pub(super) fn sccache_port(&self) -> Option<SccachePort> {
        self.sccache_port.clone()
    }
}
