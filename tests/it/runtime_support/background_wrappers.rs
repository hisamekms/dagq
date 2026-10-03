//! Session wrappers a test backend starts in the background (ADR-t1404-1
//! decision 8), each a real process: the plan review's backend runs a
//! headless planner's wrapper this way (`planner_headless`).
use anyhow::Result;
use dagq::application::ProcessControl;
use dagq::domain::background_wrapper::BackgroundHandle;
use dagq::infrastructure::adapters::SystemProcesses;
use std::{
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
};

/// One wrapper started in the background: its handle, its command line,
/// its environment and its log.
pub type BackgroundLaunch = (String, String, Vec<(String, String)>, PathBuf);

/// The wrappers a backend started, by handle; what still runs when they
/// are dropped (a test that failed) is stopped with what it started.
#[derive(Default)]
pub struct BackgroundWrappers {
    launched: Mutex<Vec<BackgroundLaunch>>,
    children: Mutex<Vec<(String, Child)>>,
}

impl BackgroundWrappers {
    /// Start `command` as a process of its own in `cwd`, with `env` over
    /// this process's environment less its dagq and cmux variables (as the
    /// cmux adapter starts it) and its output in `log`; its handle is its
    /// pid and start.
    pub fn launch(
        &self,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<String> {
        let mut process = Command::new("/bin/sh");
        process.arg("-c").arg(format!("exec {command}"));
        for (name, _) in std::env::vars() {
            if name.starts_with("DAGQ_") || name.starts_with("CMUX_") {
                process.env_remove(name);
            }
        }
        let output = fs::File::create(log)?;
        let child = process
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .process_group(0)
            .spawn()?;
        let start = SystemProcesses
            .start_identity(child.id())
            .expect("the wrapper's start");
        let handle = BackgroundHandle::new(child.id(), &start).to_string();
        self.launched.lock().unwrap().push((
            handle.clone(),
            command.to_owned(),
            env.to_vec(),
            log.to_owned(),
        ));
        self.children.lock().unwrap().push((handle.clone(), child));
        Ok(handle)
    }

    /// Every wrapper started, in order.
    pub fn launched(&self) -> Vec<BackgroundLaunch> {
        self.launched.lock().unwrap().clone()
    }

    /// Whether the wrapper `handle` still runs (its process reaped once it
    /// ended).
    pub fn runs(&self, handle: &str) -> bool {
        self.children
            .lock()
            .unwrap()
            .iter_mut()
            .any(|(id, child)| id == handle && matches!(child.try_wait(), Ok(None)))
    }

    /// Stop the wrapper `handle` and what it started, if it still runs.
    pub fn stop(&self, handle: &str) {
        for (id, child) in self.children.lock().unwrap().iter_mut() {
            if id == handle {
                stop(child);
            }
        }
    }
}

impl Drop for BackgroundWrappers {
    fn drop(&mut self) {
        for (_, child) in self.children.lock().unwrap().iter_mut() {
            stop(child);
        }
    }
}

/// `child` and its descendants, killed while it runs, and reaped.
fn stop(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        for pid in SystemProcesses.descendants(child.id()) {
            let _ = SystemProcesses.kill(pid);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
