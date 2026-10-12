//! The path of a run's container to the queue broker ([Container runtime],
//! ADR-t2114-3 decisions 2 to 4): the relay `dagq relay serve` runs in the
//! dagq machine ([`serve_relay`]: the run's unix socket to the broker's
//! port), the forwarder `dagq relay forward` runs in the run's container
//! ([`serve_forward`]: `127.0.0.1:<port>` to the run's socket), both of
//! which carry each connection's bytes both ways as they are and hold no
//! token; the relays of a queue in the machine, through podman
//! ([`PodmanRelays`]); and the supervisor's record of them that the gate
//! reads ([`QueueRelayHealth`]).
//!
//! A queue's open relays are remembered in `<queue dir>/relay/` (mode
//! 0700), one `<run>.json` with the relay's [`RelaySpec`] each, so a relay
//! whose container went with a stopped machine is still looked at and
//! started again. Every podman command names the dagq machine
//! (`podman --connection dagq`).
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::podman_machine::{PodmanCli, QueueMachine};
use super::queue_service::{private_dir, queue_dir_of, write_private};
use super::sqlite::SqliteQueue;
use crate::application::broker_relay::{RelayControl, RelayRecords, down_message};
use crate::domain::broker_relay::{
    RelaySeen, RelaySpec, SOCKET_MODE, TARGET_LABEL, relay_run_args, socket_path,
};
use crate::domain::podman_machine::QueueKey;

/// The directory of a queue's open relays under its directory.
pub const RELAY_DIR: &str = "relay";
/// How long a connection to the broker or the run's socket may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// --- Carrying bytes ------------------------------------------------------------

/// One end of a connection the relay or the forwarder carries.
trait End: Read + Write + Send + Sized + 'static {
    fn split(&self) -> io::Result<Self>;
    fn close_write(&self);
}

impl End for TcpStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl End for UnixStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

/// Carry the bytes between `a` and `b` both ways as they are, each way
/// closed for writing once the other side has no more, until both end.
fn splice<A: End, B: End>(a: A, b: B) -> io::Result<()> {
    let (mut a_in, mut b_out) = (a.split()?, b.split()?);
    let up = thread::spawn(move || {
        let _ = io::copy(&mut a_in, &mut b_out);
        b_out.close_write();
    });
    let (mut a, mut b) = (a, b);
    let _ = io::copy(&mut b, &mut a);
    a.close_write();
    let _ = up.join();
    Ok(())
}

/// Accept on `accept` until `stop` is set, carrying each connection to the
/// one `connect` makes on its own thread; a connection `connect` cannot
/// make is closed (the client sees nothing answer). How many were taken.
fn carry<S: End, D: End>(
    mut accept: impl FnMut() -> io::Result<S>,
    connect: impl Fn() -> io::Result<D> + Send + Sync + 'static,
    stop: &AtomicBool,
    poll: Duration,
) -> u64 {
    let connect = Arc::new(connect);
    let mut served = 0u64;
    while !stop.load(Ordering::SeqCst) {
        match accept() {
            Ok(stream) => {
                served += 1;
                let connect = connect.clone();
                thread::spawn(move || match connect() {
                    Ok(to) => {
                        let _ = splice(stream, to);
                    }
                    Err(error) => warn!("the relay could not pass a connection on: {error}"),
                });
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(poll),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => {
                warn!("the relay could not accept: {error}");
                thread::sleep(poll);
            }
        }
    }
    served
}

/// Bind the run's socket at `path` in place of what is there, with mode
/// [`SOCKET_MODE`], and give it to `owner` when that is another uid (the
/// run's container's user; the relay runs as root in its container for
/// this alone).
pub fn bind_socket(path: &Path, owner: Option<u32>) -> Result<UnixListener> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)
            .with_context(|| format!("remove the old socket {}", path.display()))?,
        Ok(_) => bail!("{} is there and is no socket", path.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("look at {}", path.display())),
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))
        .with_context(|| format!("narrow {}", path.display()))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if let Some(uid) = owner.filter(|uid| *uid != me) {
        std::os::unix::fs::lchown(path, Some(uid), None)
            .with_context(|| format!("give {} to uid {uid}", path.display()))?;
    }
    Ok(listener)
}

/// The relay on `listener` (the run's socket) to the broker at `target`
/// until `stop` is set: how many connections it carried.
pub fn relay_on(listener: &UnixListener, target: &str, stop: &AtomicBool, poll: Duration) -> u64 {
    let target = target.to_owned();
    carry(
        || {
            let (stream, _) = listener.accept()?;
            stream.set_nonblocking(false)?;
            Ok(stream)
        },
        move || {
            let address = std::net::ToSocketAddrs::to_socket_addrs(&target)?
                .next()
                .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no address"))?;
            TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        },
        stop,
        poll,
    )
}

/// The forwarder on `listener` (`127.0.0.1:<port>` in the run's container)
/// to the run's socket at `socket` until `stop` is set: how many
/// connections it carried.
pub fn forward_on(listener: &TcpListener, socket: &Path, stop: &AtomicBool, poll: Duration) -> u64 {
    let socket = socket.to_path_buf();
    carry(
        || {
            let (stream, _) = listener.accept()?;
            stream.set_nonblocking(false)?;
            Ok(stream)
        },
        move || UnixStream::connect(&socket),
        stop,
        poll,
    )
}

/// How `dagq relay serve` runs.
pub struct RelayOptions {
    pub socket: PathBuf,
    /// The broker's `<host>:<port>`.
    pub target: String,
    pub owner: Option<u32>,
    pub stop: Arc<AtomicBool>,
    pub poll: Duration,
}

/// Run the relay of a run until `stop` is set: bind its socket, carry each
/// connection to the broker, and remove the socket at the end.
pub fn serve_relay(options: &RelayOptions) -> Result<Value> {
    let listener = bind_socket(&options.socket, options.owner)?;
    listener.set_nonblocking(true)?;
    info!(socket = %options.socket.display(), target = %options.target, "the relay listens");
    let served = relay_on(&listener, &options.target, &options.stop, options.poll);
    let _ = fs::remove_file(&options.socket);
    Ok(json!({"outcome": "stopped", "served": served}))
}

/// How `dagq relay forward` runs.
pub struct ForwardOptions {
    pub port: u16,
    pub socket: PathBuf,
    pub stop: Arc<AtomicBool>,
    pub poll: Duration,
}

/// Run the forwarder of a run's container until `stop` is set.
pub fn serve_forward(options: &ForwardOptions) -> Result<Value> {
    let listener = TcpListener::bind(("127.0.0.1", options.port))
        .with_context(|| format!("listen on 127.0.0.1:{}", options.port))?;
    listener.set_nonblocking(true)?;
    info!(port = options.port, socket = %options.socket.display(), "the forwarder listens");
    let served = forward_on(&listener, &options.socket, &options.stop, options.poll);
    Ok(json!({"outcome": "stopped", "served": served}))
}

// --- The relays of a queue in the dagq machine ----------------------------------

/// The relays of the queue in `queue_dir`, in the dagq machine.
#[derive(Debug, Clone)]
pub struct PodmanRelays {
    pub podman: PodmanCli,
    pub queue_dir: PathBuf,
    /// The machine as this queue uses it: started again under its lock.
    pub machine: QueueMachine,
}

impl PodmanRelays {
    /// The relays of the queue whose DB is `db`, the queues being beside it
    /// under the data home and the machine's resources those of
    /// `host_wide`.
    pub fn of_queue(podman: PodmanCli, db: &Path, host_wide: Option<&Path>) -> Self {
        let queue_dir = queue_dir_of(db);
        let data_root = queue_dir.parent().unwrap_or(Path::new(".")).to_path_buf();
        Self {
            machine: QueueMachine::new(podman.clone(), &data_root, &queue_dir, host_wide),
            podman,
            queue_dir,
        }
    }

    fn key(&self) -> Result<QueueKey> {
        let name = self
            .queue_dir
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("{} names no queue", self.queue_dir.display()))?;
        QueueKey::new(name).map_err(anyhow::Error::msg)
    }

    fn dir(&self) -> PathBuf {
        self.queue_dir.join(RELAY_DIR)
    }

    fn record(&self, run: &str) -> Result<PathBuf> {
        // The run is checked as a podman name's part before it names a file.
        self.key()?
            .relay_container(run)
            .map_err(anyhow::Error::msg)?;
        Ok(self.dir().join(format!("{run}.json")))
    }

    fn specs(&self) -> Result<Vec<RelaySpec>> {
        let entries = match fs::read_dir(self.dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("read the queue's relays"),
        };
        let mut specs = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
                specs.push(
                    serde_json::from_slice(&bytes)
                        .with_context(|| format!("read {}", path.display()))?,
                );
            }
        }
        specs.sort_by(|a: &RelaySpec, b| a.run.cmp(&b.run));
        Ok(specs)
    }

    /// `podman --connection dagq <args>` to success.
    fn run(&self, args: &[String]) -> Result<String> {
        let output = self.podman.inside(args)?;
        if !output.status.success() {
            bail!(
                "podman --connection dagq {} exited with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Start the relay of `spec` towards `port`, in place of the one there.
    fn start(&self, spec: &RelaySpec, port: u16) -> Result<()> {
        let key = self.key()?;
        let container = key.relay_container(&spec.run).map_err(anyhow::Error::msg)?;
        let volume = key.socket_volume(&spec.run).map_err(anyhow::Error::msg)?;
        self.run(&strings(&["rm", "--force", "--ignore", &container]))?;
        self.run(&strings(&[
            "volume",
            "create",
            "--ignore",
            "--label",
            &format!(
                "{}={}",
                crate::domain::broker_relay::QUEUE_LABEL,
                key.as_str()
            ),
            &volume,
        ]))?;
        self.run(&relay_run_args(&key, spec, port).map_err(anyhow::Error::msg)?)
            .map(drop)
    }

    /// Whether the relay `container` runs, and the broker's port it was
    /// started towards; a container that is not there does not run.
    fn look(&self, container: &str) -> (bool, Option<u16>) {
        let format =
            format!("{{{{.State.Running}}}} {{{{index .Config.Labels \"{TARGET_LABEL}\"}}}}");
        match self.podman.inside(&strings(&[
            "container",
            "inspect",
            "--format",
            &format,
            container,
        ])) {
            Ok(output) if output.status.success() => {
                parse_inspect(&String::from_utf8_lossy(&output.stdout))
            }
            _ => (false, None),
        }
    }

    /// Whether a request through the relay's socket reaches the broker's
    /// health, asked inside the relay's container as the socket's owner
    /// `uid`: the relay's root holds no capability to open another's 0600
    /// socket.
    fn reaches(&self, container: &str, uid: u32) -> bool {
        let uid = uid.to_string();
        let mut args = strings(&[
            "exec", "--user", &uid, container, "dagq", "relay", "probe", "--socket",
        ]);
        args.push(socket_path());
        self.podman
            .inside(&args)
            .is_ok_and(|output| output.status.success())
    }
}

/// `.State.Running` and the target label as `podman container inspect
/// --format` prints them.
pub fn parse_inspect(text: &str) -> (bool, Option<u16>) {
    let mut words = text.split_whitespace();
    let running = words.next() == Some("true");
    (running, words.next().and_then(|port| port.parse().ok()))
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

impl RelayControl for PodmanRelays {
    fn open(&self, spec: &RelaySpec, port: u16) -> Result<()> {
        let record = self.record(&spec.run)?;
        private_dir(&self.dir())?;
        write_private(&record, &serde_json::to_vec(spec)?)?;
        if let Err(error) = self.start(spec, port) {
            let _ = self.close(&spec.run);
            return Err(error);
        }
        Ok(())
    }

    fn close(&self, run: &str) -> Result<()> {
        let record = self.record(run)?;
        let key = self.key()?;
        let container = key.relay_container(run).map_err(anyhow::Error::msg)?;
        let volume = key.socket_volume(run).map_err(anyhow::Error::msg)?;
        self.run(&strings(&["rm", "--force", "--ignore", &container]))?;
        let exists = self
            .podman
            .inside(&strings(&["volume", "exists", &volume]))?;
        if exists.status.success() {
            self.run(&strings(&["volume", "rm", "--force", &volume]))?;
        }
        match fs::remove_file(&record) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", record.display())),
        }
    }

    fn relays(&self) -> Result<Vec<RelaySeen>> {
        let specs = self.specs()?;
        if specs.is_empty() {
            return Ok(Vec::new());
        }
        let key = self.key()?;
        specs
            .into_iter()
            .map(|spec| {
                let container = key.relay_container(&spec.run).map_err(anyhow::Error::msg)?;
                let (running, target_port) = self.look(&container);
                Ok(RelaySeen {
                    reaches: running.then(|| self.reaches(&container, spec.uid)),
                    run: spec.run,
                    running,
                    target_port,
                })
            })
            .collect()
    }

    fn restart(&self, run: &str, port: u16) -> Result<()> {
        let record = self.record(run)?;
        let bytes = fs::read(&record).with_context(|| format!("read {}", record.display()))?;
        let spec: RelaySpec =
            serde_json::from_slice(&bytes).with_context(|| format!("read {}", record.display()))?;
        self.start(&spec, port)
    }

    fn restart_machine(&self) -> Result<()> {
        crate::application::podman_machine::restart(&self.machine.machine())
    }
}

/// The supervisor's record of the relays, read on the queue's DB.
#[derive(Debug, Clone)]
pub struct QueueRelayHealth {
    pub db: PathBuf,
}

impl RelayRecords for QueueRelayHealth {
    fn down(&self) -> Result<Option<String>> {
        down_message(&SqliteQueue::open_read_only(&self.db)?)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    const POLL: Duration = Duration::from_millis(10);

    /// Run `serve` on its own thread until the returned flag is set.
    fn running(
        serve: impl FnOnce(Arc<AtomicBool>) -> u64 + Send + 'static,
    ) -> (Arc<AtomicBool>, thread::JoinHandle<u64>) {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        (stop, thread::spawn(move || serve(flag)))
    }

    /// A peer that says `hi:` first, then sends back what it reads until
    /// the other side stops writing: both ways of a connection at once.
    fn greet_and_echo(mut stream: impl Read + Write) {
        stream.write_all(b"hi:").unwrap();
        let mut read = Vec::new();
        stream.read_to_end(&mut read).unwrap();
        stream.write_all(&read).unwrap();
    }

    fn ask(mut stream: impl End) -> String {
        stream.write_all(b"hello").unwrap();
        stream.close_write();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    }

    #[test]
    fn the_forwarder_carries_a_loopback_connection_to_the_socket_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("b.sock");
        let broker = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = broker.accept().unwrap();
                greet_and_echo(stream);
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let path = socket.clone();
        let (stop, forwarder) = running(move |stop| forward_on(&listener, &path, &stop, POLL));
        for _ in 0..2 {
            let stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            assert_eq!(ask(stream), "hi:hello");
        }
        peer.join().unwrap();
        stop.store(true, Ordering::SeqCst);
        assert_eq!(forwarder.join().unwrap(), 2);
    }

    #[test]
    fn the_relay_s_socket_is_its_owner_s_alone_and_carries_to_the_broker_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("r.sock");
        // A file left by a relay before is replaced; anything else is not.
        UnixListener::bind(&socket).unwrap();
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        let listener = bind_socket(&socket, Some(me)).unwrap();
        let metadata = fs::metadata(&socket).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, SOCKET_MODE);
        assert_eq!(metadata.uid(), me);
        let plain = dir.path().join("plain");
        fs::write(&plain, "").unwrap();
        assert!(bind_socket(&plain, None).is_err());

        let broker = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = broker.local_addr().unwrap().to_string();
        let peer = thread::spawn(move || {
            let (stream, _) = broker.accept().unwrap();
            greet_and_echo(stream);
        });
        listener.set_nonblocking(true).unwrap();
        let (stop, relay) = running(move |stop| relay_on(&listener, &target, &stop, POLL));
        let stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(ask(stream), "hi:hello");
        peer.join().unwrap();
        stop.store(true, Ordering::SeqCst);
        assert_eq!(relay.join().unwrap(), 1);
    }

    #[test]
    fn the_socket_s_owner_is_the_user_the_base_image_runs_as() {
        let uid = crate::domain::broker_relay::CONTAINER_UID;
        let base = super::super::container_image::BASE_CONTAINERFILE;
        assert!(base.contains(&format!("useradd --uid {uid} ")), "{base}");
    }

    #[test]
    fn an_inspect_reads_whether_the_relay_runs_and_its_target() {
        assert_eq!(parse_inspect("true 4321\n"), (true, Some(4321)));
        assert_eq!(parse_inspect("false 4321"), (false, Some(4321)));
        assert_eq!(parse_inspect("true <no value>"), (true, None));
        assert_eq!(parse_inspect(""), (false, None));
    }
}
