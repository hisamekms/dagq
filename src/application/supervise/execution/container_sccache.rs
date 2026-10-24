//! The sccache server of a worker's turn in its container (ADR-t2114-4
//! decision 1, ADR-t2257-1 decision 5): only the supervisor starts it. On
//! each pass it looks, for each run it leases in a clone, for the request
//! the container's launcher leaves while it waits for its gate, and answers
//! it on a thread of its own ([`serve_sccache_request`]), one at a time for
//! each run, so that no `podman` call holds the pass.

use super::*;
use crate::application::run_container::serve_sccache_request;

/// The answers to the runs' requests that are under way.
#[derive(Default)]
pub(super) struct ContainerServers {
    answering: HashMap<RunId, thread::JoinHandle<()>>,
}

impl Supervisor<'_> {
    /// Answer the request of each run in a clone this process leases whose
    /// launcher left one, unless one of the run's is being answered.
    pub(super) fn answer_sccache_requests(&mut self) {
        self.container_servers
            .answering
            .retain(|_, answer| !answer.is_finished());
        for slot in self.claim.slots.iter() {
            let run = &slot.run;
            if !run.in_container() || self.container_servers.answering.contains_key(run.id()) {
                continue;
            }
            let Some(run_dir) = run.run_dir() else {
                continue;
            };
            if !self
                .run_containers
                .sccache_requests(Path::new(run_dir))
                .is_ok_and(|requests| !requests.is_empty())
            {
                continue;
            }
            let containers = self.run_containers.clone();
            let queues = self.queues.clone();
            let id = run.id().clone();
            let run = run.clone();
            let at = self.generators.clock.now();
            let answer = thread::spawn(move || {
                let answered = queues
                    .open()
                    .and_then(|queue| serve_sccache_request(&*containers, &*queue, &run, at));
                if let Err(error) = answered {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the request for its container's sccache server was not answered: {error:#}", run.id());
                }
            });
            self.container_servers.answering.insert(id, answer);
        }
    }
}
