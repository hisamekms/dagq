//! One process read from the OS itself: its parent, its start, whether it
//! is a zombie and its command line. macOS answers through `sysctl`
//! (`KERN_PROC_PID`, `KERN_PROCARGS2`), Linux through `/proc`.
//!
//! No `ps` starts here, as Codex's workspace-write sandbox refuses the exec
//! of the setuid `/bin/ps` while it lets these calls through: measured on
//! macOS, `KERN_PROC_PID` reads any process, zombies included, and
//! `KERN_PROCARGS2` itself and its children, others not being measured
//! (docs/design/security.md, "Codexのworkerのsandbox").
//! `proc_pidinfo` would not do on macOS: it answers ESRCH for a zombie,
//! whose start `ps` prints and the callers still read.
//!
//! The start is written as `ps -o lstart=` prints it in the C locale, the
//! text a run's wrapper handle and a session's identifier already hold, so
//! it is the same across an update of the binary. The two readings differ
//! in their zone and must stay apart: [`start_in_local_zone`] is what a
//! stored [`crate::domain::background_wrapper::BackgroundHandle`] holds, and
//! in UTC it would no longer match and a live wrapper would read as dead;
//! [`start_in_utc`] is what the queue holds as a session's identifier.
//!
//! What the system calls return is read by functions without side effects,
//! on every platform, so that the tests of either format run on both.

use crate::domain::host_metrics::civil;

/// What the OS says of one process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRead {
    pub pid: u32,
    pub ppid: u32,
    /// When it started, in unix seconds.
    pub started_secs: i64,
    /// It has ended and its parent has not waited for it yet.
    pub zombie: bool,
}

/// The process `pid` now, a zombie included; `None` when it runs nothing.
pub fn read_process(pid: u32) -> Option<ProcessRead> {
    read_from_os(pid)
}

/// The command line of `pid`, its arguments joined by spaces as
/// `ps -o command=` prints them, bytes that are not UTF-8 replaced; `None`
/// when it runs nothing, holds no command line (a zombie) or is not this
/// user's.
pub fn read_command(pid: u32) -> Option<String> {
    command_from_os(pid)
}

/// When `pid` started, in the zone of this process's environment: what
/// `LC_ALL=C ps -o lstart= -p <pid>` prints.
pub fn start_in_local_zone(pid: u32) -> Option<String> {
    let started = read_process(pid)?.started_secs;
    Some(start_text(started, utc_offset_at(started)?))
}

/// When `pid` started, in UTC whatever the environment names: what
/// `LC_ALL=C TZ=UTC0 ps -o lstart= -p <pid>` prints.
pub fn start_in_utc(pid: u32) -> Option<String> {
    Some(start_text(read_process(pid)?.started_secs, 0))
}

/// The unix second `secs`, in a zone `utc_offset_secs` east of UTC, as
/// `%a %b %e %H:%M:%S %Y` in the C locale writes it: `Wed Nov  4 08:08:21
/// 2026`. The names are written here, not asked of a locale.
pub fn start_text(secs: i64, utc_offset_secs: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let local = secs + utc_offset_secs;
    let days = local.div_euclid(86_400);
    let second = local.rem_euclid(86_400);
    let (year, month, date) = civil(days);
    // Day 0, 1970-01-01, was a Thursday.
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    let month = MONTHS[(month - 1) as usize];
    let (hour, minute, second) = (second / 3_600, second / 60 % 60, second % 60);
    format!("{weekday} {month} {date:>2} {hour:02}:{minute:02}:{second:02} {year}")
}

/// Whether macOS's `p_stat` says zombie (`SZOMB`).
pub const fn zombie_on_macos(p_stat: u8) -> bool {
    p_stat == 5
}

/// Whether the state field of Linux's `/proc/<pid>/stat` says zombie.
pub const fn zombie_on_linux(state: char) -> bool {
    matches!(state, 'Z')
}

/// The size of macOS's `struct kinfo_proc`, and the offsets in it of what
/// is read: libc does not declare the struct, and the layout is the same
/// on arm64 and x86_64.
const KINFO_PROC_SIZE: usize = 648;
const KINFO_STARTED_SECS: usize = 0;
const KINFO_STAT: usize = 36;
const KINFO_PID: usize = 40;
const KINFO_PPID: usize = 560;

/// The process in what `sysctl(KERN_PROC_PID)` wrote; `None` for anything
/// shorter than the struct, such as the empty answer for a pid that runs
/// nothing.
pub fn process_from_kinfo(kinfo: &[u8]) -> Option<ProcessRead> {
    if kinfo.len() < KINFO_PROC_SIZE {
        return None;
    }
    let word = |at: usize| Some(i32::from_ne_bytes(kinfo[at..at + 4].try_into().ok()?));
    Some(ProcessRead {
        pid: u32::try_from(word(KINFO_PID)?).ok()?,
        ppid: u32::try_from(word(KINFO_PPID)?).ok()?,
        started_secs: i64::from_ne_bytes(
            kinfo[KINFO_STARTED_SECS..KINFO_STARTED_SECS + 8]
                .try_into()
                .ok()?,
        ),
        zombie: zombie_on_macos(kinfo[KINFO_STAT]),
    })
}

/// The process in the text of `/proc/<pid>/stat`, with `/proc/stat` for the
/// boot time and the clock ticks per second that its start counts in. The
/// command name is in parentheses and may hold spaces and parentheses
/// itself, so the fields after it are read from the last `)`.
pub fn process_from_proc(stat: &str, proc_stat: &str, ticks_per_sec: u64) -> Option<ProcessRead> {
    /// Where the start is among the fields after the command name: it is
    /// field 22 of proc(5), and those fields begin at field 3.
    const STARTTIME: usize = 22 - 3;
    let (head, tail) = stat.rsplit_once(')')?;
    let pid = head.split_whitespace().next()?.parse().ok()?;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let ticks = fields.get(STARTTIME)?.parse::<u64>().ok()?;
    let boot = proc_stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse::<i64>()
        .ok()?;
    let since_boot = ticks.checked_div(ticks_per_sec)?;
    Some(ProcessRead {
        pid,
        ppid: fields.get(1)?.parse().ok()?,
        started_secs: boot + i64::try_from(since_boot).ok()?,
        zombie: zombie_on_linux(fields.first()?.chars().next()?),
    })
}

/// The command line in what `sysctl(KERN_PROCARGS2)` wrote: the argument
/// count, the executable's path, padding of NULs, then the arguments, each
/// ended by a NUL, and the environment after them, which is not read.
pub fn command_from_procargs2(procargs: &[u8]) -> Option<String> {
    let count = i32::from_ne_bytes(procargs.get(..4)?.try_into().ok()?);
    let rest = procargs.get(4..)?;
    let path_end = rest.iter().position(|&byte| byte == 0)?;
    let arguments = &rest[path_end..];
    let start = arguments.iter().position(|&byte| byte != 0)?;
    let words = arguments[start..]
        .split(|&byte| byte == 0)
        .take(usize::try_from(count).ok()?);
    joined_command(words)
}

/// The command line in the bytes of `/proc/<pid>/cmdline`: arguments ended
/// by NULs, and nothing for a zombie.
pub fn command_from_cmdline(cmdline: &[u8]) -> Option<String> {
    joined_command(
        cmdline
            .strip_suffix(&[0])
            .unwrap_or(cmdline)
            .split(|&byte| byte == 0),
    )
}

fn joined_command<'a>(words: impl Iterator<Item = &'a [u8]>) -> Option<String> {
    let command = words
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    (!command.is_empty()).then_some(command)
}

/// How far east of UTC the zone of this process's environment is at the
/// unix second `secs`, summer time included as it was then.
fn utc_offset_at(secs: i64) -> Option<i64> {
    let at = secs as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both pointers are valid for the call, which keeps neither;
    // localtime_r(3) writes only the `tm` it is given.
    let filled = unsafe { libc::localtime_r(&at, local.as_mut_ptr()) };
    if filled.is_null() {
        return None;
    }
    // SAFETY: localtime_r succeeded, so `local` is filled, and a zeroed
    // `tm` is valid anyway (integers and a null pointer).
    Some(unsafe { local.assume_init() }.tm_gmtoff)
}

/// What `sysctl` writes for the name `mib`, up to `capacity` bytes; `None`
/// when it refuses.
#[cfg(target_os = "macos")]
fn sysctl_bytes(mib: &mut [libc::c_int], capacity: usize) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; capacity];
    let mut written = capacity;
    // SAFETY: `mib` and the buffer are valid for their lengths during the
    // call, which writes at most `written` bytes, sets `written` to what it
    // wrote and keeps no pointer; no new value is passed.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            libc::c_uint::try_from(mib.len()).ok()?,
            buffer.as_mut_ptr().cast(),
            &mut written,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(written);
    Some(buffer)
}

#[cfg(target_os = "macos")]
fn read_from_os(pid: u32) -> Option<ProcessRead> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    process_from_kinfo(&sysctl_bytes(&mut mib, KINFO_PROC_SIZE)?)
}

#[cfg(target_os = "macos")]
fn command_from_os(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let limit = sysctl_bytes(
        &mut [libc::CTL_KERN, libc::KERN_ARGMAX],
        size_of::<libc::c_int>(),
    )?;
    let limit = usize::try_from(i32::from_ne_bytes(limit.try_into().ok()?)).ok()?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    command_from_procargs2(&sysctl_bytes(&mut mib, limit)?)
}

#[cfg(not(target_os = "macos"))]
fn read_from_os(pid: u32) -> Option<ProcessRead> {
    // SAFETY: sysconf(3) has no memory effects.
    let ticks_per_sec = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    // Bytes, not text: a command name need not be UTF-8.
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let proc_stat = std::fs::read_to_string("/proc/stat").ok()?;
    process_from_proc(&String::from_utf8_lossy(&stat), &proc_stat, ticks_per_sec)
}

#[cfg(not(target_os = "macos"))]
fn command_from_os(pid: u32) -> Option<String> {
    command_from_cmdline(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-04 08:08:21 UTC, a Wednesday.
    const NOV_4: i64 = 1_793_779_701;

    #[test]
    fn a_start_is_written_as_ps_prints_it_in_utc_and_in_a_given_zone() {
        assert_eq!(start_text(NOV_4, 0), "Wed Nov  4 08:08:21 2026");
        assert_eq!(start_text(NOV_4, 9 * 3_600), "Wed Nov  4 17:08:21 2026");
        assert_eq!(start_text(NOV_4, -9 * 3_600), "Tue Nov  3 23:08:21 2026");
        // A two-digit date takes the room of the space.
        assert_eq!(
            start_text(NOV_4 + 10 * 86_400, 0),
            "Sat Nov 14 08:08:21 2026"
        );
        assert_eq!(start_text(0, 0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(start_text(-1, 0), "Wed Dec 31 23:59:59 1969");
    }

    #[test]
    fn zombies_are_told_by_the_stat_values_of_each_system() {
        assert!(zombie_on_macos(5));
        for running in [1, 2, 3, 4] {
            assert!(!zombie_on_macos(running));
        }
        assert!(zombie_on_linux('Z'));
        for running in ['R', 'S', 'D', 'T'] {
            assert!(!zombie_on_linux(running));
        }
    }

    const PROC_STAT: &str = "cpu  1 2 3\nintr 5 6\nbtime 1793000000\nprocesses 7\n";

    fn proc_text(name: &str, state: char) -> String {
        format!(
            "4242 ({name}) {state} 17 4242 4242 0 -1 4194304 1 0 0 0 1 1 0 0 20 0 1 0 123456 \
             1000 10 18446744073709551615"
        )
    }

    #[test]
    fn proc_text_reads_past_a_name_with_parentheses_and_spaces() {
        let read = process_from_proc(&proc_text("a) b (c) 9 9", 'S'), PROC_STAT, 100);
        assert_eq!(
            read,
            Some(ProcessRead {
                pid: 4242,
                ppid: 17,
                started_secs: 1_793_000_000 + 1_234,
                zombie: false,
            })
        );
        let zombie = process_from_proc(&proc_text("sleep", 'Z'), PROC_STAT, 100);
        assert!(zombie.unwrap().zombie);
    }

    #[test]
    fn proc_text_that_does_not_read_is_nothing() {
        let stat = proc_text("sleep", 'S');
        assert_eq!(process_from_proc("", PROC_STAT, 100), None);
        assert_eq!(process_from_proc("4242 (sleep) S 17", PROC_STAT, 100), None);
        assert_eq!(process_from_proc(&stat, "cpu 1 2 3\n", 100), None);
        assert_eq!(process_from_proc(&stat, PROC_STAT, 0), None);
    }

    #[test]
    fn a_kinfo_reads_its_fields_and_an_empty_answer_is_nothing() {
        let mut kinfo = vec![0u8; KINFO_PROC_SIZE];
        kinfo[KINFO_STARTED_SECS..8].copy_from_slice(&NOV_4.to_ne_bytes());
        kinfo[KINFO_STAT] = 5;
        kinfo[KINFO_PID..KINFO_PID + 4].copy_from_slice(&4242i32.to_ne_bytes());
        kinfo[KINFO_PPID..KINFO_PPID + 4].copy_from_slice(&17i32.to_ne_bytes());
        assert_eq!(
            process_from_kinfo(&kinfo),
            Some(ProcessRead {
                pid: 4242,
                ppid: 17,
                started_secs: NOV_4,
                zombie: true,
            })
        );
        assert_eq!(process_from_kinfo(&[]), None);
    }

    #[test]
    fn procargs_give_the_arguments_without_the_path_or_the_environment() {
        let mut procargs = 3i32.to_ne_bytes().to_vec();
        procargs.extend_from_slice(b"/bin/dagq\0\0\0\0dagq\0service\0serve\0HOME=/h\0TZ=UTC\0");
        assert_eq!(
            command_from_procargs2(&procargs).as_deref(),
            Some("dagq service serve")
        );
        assert_eq!(command_from_procargs2(&[]), None);
        assert_eq!(command_from_procargs2(&3i32.to_ne_bytes()), None);
    }

    #[test]
    fn a_cmdline_joins_its_arguments_and_a_zombie_has_none() {
        assert_eq!(
            command_from_cmdline(b"dagq\0service\0serve\0").as_deref(),
            Some("dagq service serve")
        );
        assert_eq!(command_from_cmdline(b""), None);
        assert_eq!(
            command_from_cmdline(b"a\xff\0b\0").as_deref(),
            Some("a\u{fffd} b")
        );
    }

    #[test]
    fn this_process_reads_with_its_parent_and_a_pid_that_runs_nothing_does_not() {
        let own = read_process(std::process::id()).unwrap();
        assert_eq!(own.pid, std::process::id());
        assert_eq!(own.ppid, std::os::unix::process::parent_id());
        assert!(!own.zombie);
        assert!(start_in_local_zone(own.pid).is_some());
        assert_eq!(start_in_utc(own.pid), Some(start_text(own.started_secs, 0)));

        assert_eq!(read_process(u32::MAX), None);
        assert_eq!(start_in_local_zone(u32::MAX), None);
        assert_eq!(start_in_utc(u32::MAX), None);
        assert_eq!(read_command(u32::MAX), None);
    }

    #[test]
    fn this_process_reads_its_own_command_line() {
        let first = std::env::args().next().unwrap();
        let command = read_command(std::process::id()).unwrap();
        assert!(command.starts_with(&first), "{command} against {first}");
    }
}
