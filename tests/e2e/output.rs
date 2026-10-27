//! The readers of a child's stdout and stderr: what they read sits in a
//! shared buffer, so a failure can print it without waiting for EOF, which
//! never comes while a descendant of the child (a review, a throughput
//! review) still holds the pipe open.

use std::{
    io::{self, Read},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// How long [`OutputReader::snapshot`] waits for the reader to reach EOF, so
/// the last lines a child wrote before it exited are read in the usual case.
const DRAIN_LIMIT: Duration = Duration::from_secs(1);

/// Keep each read available even while a descendant holds the pipe open.
/// Never hold the buffer lock during a blocking read or thread join.
pub(super) struct OutputReader {
    bytes: Arc<Mutex<Vec<u8>>>,
    handle: thread::JoinHandle<io::Result<()>>,
}

pub(super) fn reader(mut source: impl Read + Send + 'static) -> OutputReader {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&bytes);
    let handle = thread::spawn(move || {
        let mut chunk = [0; 8192];
        loop {
            match source.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(n) => captured.lock().unwrap().extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    });
    OutputReader { bytes, handle }
}

impl OutputReader {
    /// What was read so far, for a failure: it waits for EOF at most
    /// [`DRAIN_LIMIT`] (and dropping the reader never joins it).
    pub(super) fn snapshot(&self) -> String {
        let deadline = Instant::now() + DRAIN_LIMIT;
        while !self.handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        String::from_utf8_lossy(&self.bytes.lock().unwrap()).into_owned()
    }

    /// Successful paths still consume the complete output before assertions.
    pub(super) fn finish(self, what: &str) -> String {
        super::joined(self.handle, what).unwrap();
        String::from_utf8(self.bytes.lock().unwrap().clone()).unwrap()
    }
}

#[test]
fn snapshot_reports_partial_output_without_eof() {
    use std::{io::Write, os::unix::net::UnixStream, time::Duration};

    let _waiting = super::common::within(Duration::from_secs(10), "partial output regression");
    let (source, mut writer) = UnixStream::pair().unwrap();
    let output = reader(source);
    // No newline, and even an incomplete UTF-8 character, must be visible.
    writer.write_all(b"supervisor diagnostic: \xe3").unwrap();
    while output.snapshot() != "supervisor diagnostic: \u{fffd}" {
        thread::yield_now();
    }
    // Unwinding drops the reader while the writer still holds the pipe open.
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        panic!("timeout: {}", output.snapshot());
    }));
    let message = failure.unwrap_err().downcast::<String>().unwrap();
    assert!(message.contains("supervisor diagnostic:"));
    // The writer is still open: neither snapshot nor dropping the reader joins.
    writer.write_all(b"\x81\x82").unwrap();
    drop(writer);
    // No process was spawned; the detached reader can now reach EOF.
}

#[test]
fn finish_reads_all_output_including_split_utf8() {
    use std::{io::Write, os::unix::net::UnixStream};

    let (source, mut writer) = UnixStream::pair().unwrap();
    let output = reader(source);
    let writing = thread::spawn(move || {
        writer.write_all(b"first \xe3").unwrap();
        writer.write_all(b"\x81\x82 last").unwrap();
    });
    assert_eq!(output.finish("complete output reader"), "first あ last");
    writing.join().unwrap();
}
