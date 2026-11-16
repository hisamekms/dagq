//! Ordering at the binary port: a competing queue cannot replace the
//! target between keeping its old binary and moving the new one there.

use super::{test_support::*, *};
use crate::application::memory_files::MemoryFiles;
use crate::domain::replan_snapshot::sha256;
use std::sync::Mutex;

struct Target {
    held: bool,
    version: String,
}

struct Held(Arc<Mutex<Target>>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.lock().unwrap().held = false;
    }
}

/// Refuse a conflicting acquisition rather than block the test's only
/// thread. The real adapter waits on a flock of the target's stable sibling.
struct Contending<'a> {
    target: Arc<Mutex<Target>>,
    after_keep: Option<&'a dyn Fn()>,
}

impl Binaries for Contending<'_> {
    fn hold_target(&self, target: &Path) -> Result<Box<dyn std::any::Any + Send>> {
        assert_eq!(target, Path::new("/to"));
        let mut state = self.target.lock().unwrap();
        ensure!(!state.held, "the target is locked by another install");
        state.held = true;
        Ok(Box::new(Held(self.target.clone())))
    }
    fn build(&self, _: &Path) -> Result<PathBuf> {
        bail!("already built")
    }
    fn version(&self, path: &Path) -> Result<String> {
        Ok(if path == Path::new("/to") {
            self.target.lock().unwrap().version.clone()
        } else {
            path.file_name().unwrap().to_str().unwrap().to_owned()
        })
    }
    fn probe(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn takes_handoff(&self, _: &Path) -> bool {
        true
    }
    fn schema(&self, _: &Path, _: &Path) -> Result<Option<SchemaCheck>> {
        bail!("no queue here")
    }
    fn serve(&self, _: &Path, _: &Path, _: bool) -> Result<Value> {
        bail!("no service here")
    }
    fn replace(&self, source: &Path, target: &Path) -> Result<()> {
        self.replace_held(source, target).map(drop)
    }
    fn replace_held(&self, source: &Path, _: &Path) -> Result<Replacement> {
        let before = {
            let target = self.target.lock().unwrap();
            assert!(
                target.held,
                "keeping the predecessor requires the target lock"
            );
            target.version.clone()
        };
        if let Some(after_keep) = self.after_keep {
            after_keep();
        }
        let version = self.version(source)?;
        self.target.lock().unwrap().version = version.clone();
        Ok(Replacement {
            digest: Some(sha256(version.as_bytes())),
            replaced_version: Some(before.clone()),
            replaced_digest: Some(sha256(before.as_bytes())),
        })
    }
    fn restore(&self, _: &Path) -> Result<()> {
        bail!("no restore here")
    }
    fn run(&self, _: &Path, _: &[String]) -> Result<Value> {
        bail!("no restart here")
    }
}

#[test]
fn an_install_holds_the_shared_target_from_keeping_the_old_binary_through_replacement() {
    let state = Arc::new(Mutex::new(Target {
        held: false,
        version: "X".into(),
    }));
    let second = Contending {
        target: state.clone(),
        after_keep: None,
    };
    let files = MemoryFiles::default();
    let queues = |_: &Path| -> Arc<dyn QueueOpener> { panic!("no queue is opened") };
    let clock = At::new(10_000);
    let install = |binaries: &dyn Binaries, version: &str| {
        install_built(
            &Ports {
                binaries,
                files: &files,
                processes: &OnlyOne,
                clock: &clock,
                queues: &queues,
                down: &|| bail!("no drain"),
                stop_records: None,
            },
            None,
            &InstallOptions {
                source: Source::Binary(PathBuf::from(version)),
                target: PathBuf::from("/to"),
                allow_breaking: false,
                restart: vec![],
                handoff_timeout: Duration::ZERO,
                poll: Duration::ZERO,
                e2e: E2eGate::NotApplicable,
            },
            Built {
                binary: PathBuf::from(version),
                e2e: json!({"status": "not_applicable"}),
            },
        )
    };
    let attempted = std::cell::Cell::new(false);
    let competing = || {
        attempted.set(true);
        let error = install(&second, "Y").unwrap_err();
        assert!(error.to_string().contains("locked by another install"));
        assert_eq!(state.lock().unwrap().version, "X");
    };
    let first = Contending {
        target: state.clone(),
        after_keep: Some(&competing),
    };
    let first = install(&first, "Z").unwrap();
    assert!(attempted.get());
    assert_eq!(first["replaced_binary"]["version"], "X");
    assert_eq!(first["replaced_binary"]["digest"], sha256(b"X"));
    assert_eq!(first["digest"], sha256(b"Z"));
    assert!(!state.lock().unwrap().held);
    // B proceeds only after A drops the guard, so its predecessor is Z.
    let second = install(&second, "Y").unwrap();
    assert_eq!(second["replaced_binary"]["version"], "Z");
    assert_eq!(second["replaced_binary"]["digest"], sha256(b"Z"));
    assert_eq!(second["digest"], sha256(b"Y"));
    assert_eq!(state.lock().unwrap().version, "Y");
    assert!(!state.lock().unwrap().held);
}
