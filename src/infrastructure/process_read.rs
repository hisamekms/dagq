//! Processes read from the OS itself: one by its pid (its parent, its start,
//! whether it is a zombie and its command line) and this user's list of them
//! ([`list_user_processes`]). macOS answers through `sysctl` (`KERN_PROC_PID`,
//! `KERN_PROCARGS2`) and `libproc`, Linux through `/proc`.
//!
//! No `ps` starts here, as Codex's workspace-write sandbox refuses the exec
//! of the setuid `/bin/ps` while it lets these calls through: measured on
//! macOS, `KERN_PROC_PID` reads any process, zombies included, and
//! `KERN_PROCARGS2` any of this user's, children or not, but not another
//! user's (docs/design/security.md, "Codexのworkerのsandbox").
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
//! The list holds what `ps -U <uid>` did, by the real uid: macOS's own list
//! by uid (`KERN_PROC_UID`) is refused in that sandbox, so every pid of
//! `proc_listallpids` is read and the other users' are left out. A process
//! that ends while the pids are read is skipped; one whose arguments or CPU
//! time do not read stays listed with its pid and parent, since the callers
//! follow the parents through it and judge a run by the list: it shows as
//! `ps` shows such a process, by its name in parentheses (brackets on Linux).
//!
//! What the system calls return is read by functions without side effects,
//! on every platform, so that the tests of either format run on both.

use anyhow::{Context as _, Result};

use crate::domain::{host_metrics::civil, recovery::ProcessInfo};

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

/// This user's processes now, with their parents, ages, CPU times and
/// command lines, at the unix second `now_secs`; no working directory is
/// read. An error when the OS gives no list of pids.
pub fn list_user_processes(now_secs: i64) -> Result<Vec<ProcessInfo>> {
    list_from_os(now_secs)
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
const KINFO_REAL_UID: usize = 392;
const KINFO_COMM: usize = 243;
const KINFO_COMM_SIZE: usize = 17;

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

/// How old a process that started at `started_secs` is at `now_secs`, in
/// seconds; none for a start the clock has not reached, as when the clock
/// was set back.
pub fn elapsed_secs(now_secs: i64, started_secs: i64) -> u64 {
    u64::try_from(now_secs.saturating_sub(started_secs)).unwrap_or(0)
}

/// A command line on one line with single spaces, as `ps` printed what the
/// callers read: an argument may hold newlines and runs of spaces.
fn one_line(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The line of a process whose arguments did not read or are empty, as
/// macOS's `ps` prints it: its name in parentheses, `<defunct>` for a zombie.
pub fn unread_command_on_macos(comm: &str, zombie: bool) -> String {
    if zombie {
        "<defunct>".to_owned()
    } else {
        format!("({comm})")
    }
}

/// The same as Linux's `ps` prints it: its name in brackets, with
/// `<defunct>` after it for a zombie.
pub fn unread_command_on_linux(comm: &str, zombie: bool) -> String {
    if zombie {
        format!("[{comm}] <defunct>")
    } else {
        format!("[{comm}]")
    }
}

/// The entry of the list for a process that read as `read`.
fn listed(
    read: &ProcessRead,
    now_secs: i64,
    command: Option<String>,
    unread: String,
    cpu_ms: Option<u64>,
) -> ProcessInfo {
    ProcessInfo {
        pid: read.pid,
        ppid: read.ppid,
        elapsed_secs: elapsed_secs(now_secs, read.started_secs),
        command: command
            .map(|command| one_line(&command))
            .filter(|command| !command.is_empty())
            .unwrap_or(unread),
        cwd: None,
        cpu_ms,
    }
}

/// The real uid in what `sysctl(KERN_PROC_PID)` wrote (`e_pcred.p_ruid`).
pub fn real_uid_from_kinfo(kinfo: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(
        kinfo
            .get(KINFO_REAL_UID..KINFO_REAL_UID + 4)?
            .try_into()
            .ok()?,
    ))
}

/// The entry of the list from what `sysctl` wrote: the struct of
/// `KERN_PROC_PID` and, when they read, the bytes of `KERN_PROCARGS2`, whose
/// words need not be UTF-8. `None` only for a struct that does not read: a
/// process without arguments keeps its pid and parent.
pub fn listed_from_kinfo(
    kinfo: &[u8],
    procargs: Option<&[u8]>,
    cpu_ms: Option<u64>,
    now_secs: i64,
) -> Option<ProcessInfo> {
    let read = process_from_kinfo(kinfo)?;
    let name = &kinfo[KINFO_COMM..KINFO_COMM + KINFO_COMM_SIZE];
    let name = name.split(|&byte| byte == 0).next().unwrap_or(name);
    let unread = unread_command_on_macos(&String::from_utf8_lossy(name), read.zombie);
    let command = procargs
        .filter(|_| !read.zombie)
        .and_then(command_from_procargs2);
    // macOS answers no task info for a zombie. It uses no more CPU, and a
    // time left unknown would keep its parents busy to the idle watch
    // ([`crate::domain::idle_process`]) for as long as it is not reaped.
    let cpu_ms = cpu_ms.or(read.zombie.then_some(0));
    Some(listed(&read, now_secs, command, unread, cpu_ms))
}

/// The real uid in the text of `/proc/<pid>/status`, the first number of its
/// `Uid:` line.
pub fn real_uid_from_status(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The entry of the list from `/proc`: the text of `/proc/<pid>/stat` and
/// `/proc/stat` as [`process_from_proc`] takes them, and the bytes of
/// `/proc/<pid>/cmdline`, which need not be UTF-8. The CPU time is left to
/// [`with_proc_cpu_time`]. `None` only for a `stat` that does not read: a
/// process without arguments keeps its pid and parent.
pub fn listed_from_proc(
    stat: &str,
    proc_stat: &str,
    ticks_per_sec: u64,
    cmdline: Option<&[u8]>,
    now_secs: i64,
) -> Option<ProcessInfo> {
    let read = process_from_proc(stat, proc_stat, ticks_per_sec)?;
    let name = stat.split_once('(')?.1.rsplit_once(')')?.0;
    let unread = unread_command_on_linux(name, read.zombie);
    let command = cmdline.and_then(command_from_cmdline);
    Some(listed(&read, now_secs, command, unread, None))
}

/// How many pids `proc_listallpids` says it has; `None` when it refused,
/// which it may tell by an answer of 0 as well as by a negative one: the list
/// always holds at least the process that asks, and a refusal must not read
/// as a host that runs nothing, which the callers would take as free to clean.
pub fn pid_count(answer: i32) -> Option<usize> {
    usize::try_from(answer).ok().filter(|&count| count > 0)
}

/// `pti_total_user` and `pti_total_system` of macOS's `proc_taskinfo`, in
/// milliseconds. They count in the units of `mach_absolute_time`, which are
/// not nanoseconds on Apple silicon: `numer / denom` is the nanoseconds per
/// unit that `mach_timebase_info` gives. `None` for a timebase of no use.
pub fn cpu_ms_from_mach(user: u64, system: u64, numer: u32, denom: u32) -> Option<u64> {
    let units = u128::from(user) + u128::from(system);
    let nanos = units
        .checked_mul(u128::from(numer))?
        .checked_div(u128::from(denom))?;
    u64::try_from(nanos / 1_000_000).ok()
}

/// `utime + stime` of a `/proc/<pid>/stat` line, in clock ticks: the 14th
/// and 15th fields, counted after the `)` that ends the command (which may
/// hold spaces and parentheses itself).
pub fn proc_stat_cpu_ticks(stat: &str) -> Option<u64> {
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace().skip(11);
    let user = fields.next()?.parse::<u64>().ok()?;
    let system = fields.next()?.parse::<u64>().ok()?;
    user.checked_add(system)
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
fn procargs_from_os(pid: libc::c_int) -> Option<Vec<u8>> {
    let limit = sysctl_bytes(
        &mut [libc::CTL_KERN, libc::KERN_ARGMAX],
        size_of::<libc::c_int>(),
    )?;
    let limit = usize::try_from(i32::from_ne_bytes(limit.try_into().ok()?)).ok()?;
    sysctl_bytes(&mut [libc::CTL_KERN, libc::KERN_PROCARGS2, pid], limit)
}

#[cfg(target_os = "macos")]
fn command_from_os(pid: u32) -> Option<String> {
    command_from_procargs2(&procargs_from_os(libc::c_int::try_from(pid).ok()?)?)
}

/// Every pid of the host; some may have ended by the time they are read.
#[cfg(target_os = "macos")]
fn all_pids() -> Result<Vec<libc::c_int>> {
    // SAFETY: a null buffer of no size asks only for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut count = pid_count(count)
        .with_context(|| format!("proc_listallpids: {}", std::io::Error::last_os_error()))?;
    loop {
        // Room for what starts between the two calls; a full buffer may
        // have left some out, and is asked for again.
        let room = count + 64;
        let mut pids: Vec<libc::c_int> = vec![0; room];
        let bytes = libc::c_int::try_from(room * size_of::<libc::c_int>())?;
        // SAFETY: the buffer is valid for `bytes` during the call, which
        // writes at most that and keeps no pointer.
        let written = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let written = pid_count(written)
            .with_context(|| format!("proc_listallpids: {}", std::io::Error::last_os_error()))?;
        if written < room {
            pids.truncate(written);
            return Ok(pids);
        }
        count = room * 2;
    }
}

/// `numer` and `denom` of the Mach clock; they do not change while the host
/// runs.
#[cfg(target_os = "macos")]
#[expect(
    deprecated,
    reason = "libc points to the mach2 crate for this one call and its struct"
)]
fn mach_timebase() -> (u32, u32) {
    let mut timebase = std::mem::MaybeUninit::<libc::mach_timebase_info>::zeroed();
    // SAFETY: the pointer is valid for the call, which only writes the
    // struct, and a zeroed one is valid should it fail.
    let timebase = unsafe {
        libc::mach_timebase_info(timebase.as_mut_ptr());
        timebase.assume_init()
    };
    (timebase.numer, timebase.denom)
}

/// The CPU time of `pid`; `None` when it does not read, as for a zombie.
#[cfg(target_os = "macos")]
fn cpu_ms_from_os(pid: libc::c_int, (numer, denom): (u32, u32)) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = libc::c_int::try_from(size_of::<libc::proc_taskinfo>()).ok()?;
    // SAFETY: the buffer is valid for `size` during the call, which writes
    // at most that and keeps no pointer.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call filled the whole struct, of integers.
    let info = unsafe { info.assume_init() };
    cpu_ms_from_mach(info.pti_total_user, info.pti_total_system, numer, denom)
}

#[cfg(target_os = "macos")]
fn list_from_os(now_secs: i64) -> Result<Vec<ProcessInfo>> {
    // SAFETY: getuid(2) has no failure and no memory effects.
    let uid = unsafe { libc::getuid() };
    let timebase = mach_timebase();
    Ok(all_pids()?
        .into_iter()
        .filter_map(|pid| {
            let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
            let kinfo = sysctl_bytes(&mut mib, KINFO_PROC_SIZE)?;
            if real_uid_from_kinfo(&kinfo)? != uid {
                return None;
            }
            let procargs = procargs_from_os(pid);
            let cpu_ms = cpu_ms_from_os(pid, timebase);
            listed_from_kinfo(&kinfo, procargs.as_deref(), cpu_ms, now_secs)
        })
        .collect())
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

#[cfg(not(target_os = "macos"))]
fn list_from_os(now_secs: i64) -> Result<Vec<ProcessInfo>> {
    // SAFETY: sysconf(3) has no memory effects.
    let ticks_per_sec = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })?;
    // SAFETY: getuid(2) has no failure and no memory effects.
    let uid = unsafe { libc::getuid() };
    let proc_stat = std::fs::read_to_string("/proc/stat").context("reading /proc/stat")?;
    let listed = std::fs::read_dir("/proc")
        .context("reading /proc")?
        .filter_map(|entry| {
            let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
            // Bytes, not text: a command name need not be UTF-8.
            let status = std::fs::read(format!("/proc/{pid}/status")).ok()?;
            if real_uid_from_status(&String::from_utf8_lossy(&status))? != uid {
                return None;
            }
            let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok();
            listed_from_proc(
                &String::from_utf8_lossy(&stat),
                &proc_stat,
                ticks_per_sec,
                cmdline.as_deref(),
                now_secs,
            )
        })
        .collect();
    Ok(with_proc_cpu_time(listed, ticks_per_sec))
}

/// `processes` with the CPU time `/proc/<pid>/stat` holds in clock ticks; a
/// process whose file does not read has none.
#[cfg(not(target_os = "macos"))]
fn with_proc_cpu_time(mut processes: Vec<ProcessInfo>, ticks_per_sec: u64) -> Vec<ProcessInfo> {
    if ticks_per_sec == 0 {
        return processes;
    }
    for process in &mut processes {
        let ticks = std::fs::read(format!("/proc/{}/stat", process.pid))
            .ok()
            .and_then(|stat| proc_stat_cpu_ticks(&String::from_utf8_lossy(&stat)));
        if let Some(ticks) = ticks {
            process.cpu_ms = Some(ticks * 1000 / ticks_per_sec);
        }
    }
    processes
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

    fn kinfo_of(pid: i32, ppid: i32, ruid: u32, comm: &[u8], stat: u8) -> Vec<u8> {
        let mut kinfo = vec![0u8; KINFO_PROC_SIZE];
        kinfo[KINFO_STARTED_SECS..8].copy_from_slice(&NOV_4.to_ne_bytes());
        kinfo[KINFO_STAT] = stat;
        kinfo[KINFO_PID..KINFO_PID + 4].copy_from_slice(&pid.to_ne_bytes());
        kinfo[KINFO_PPID..KINFO_PPID + 4].copy_from_slice(&ppid.to_ne_bytes());
        kinfo[KINFO_REAL_UID..KINFO_REAL_UID + 4].copy_from_slice(&ruid.to_ne_bytes());
        kinfo[KINFO_COMM..KINFO_COMM + comm.len()].copy_from_slice(comm);
        kinfo
    }

    #[test]
    fn a_macos_entry_reads_arguments_that_are_not_utf8_on_one_line() {
        let kinfo = kinfo_of(8, 7, 501, b"sleep", 2);
        let mut procargs = 3i32.to_ne_bytes().to_vec();
        procargs.extend_from_slice(b"/bin/x\0\0\0x\0b\xff\0two\n  lines\0HOME=/h\0");
        let entry = listed_from_kinfo(&kinfo, Some(&procargs), Some(12), NOV_4 + 65).unwrap();
        assert_eq!(
            entry,
            ProcessInfo {
                pid: 8,
                ppid: 7,
                elapsed_secs: 65,
                command: "x b\u{fffd} two lines".to_owned(),
                cwd: None,
                cpu_ms: Some(12),
            }
        );
    }

    #[test]
    fn a_macos_process_whose_arguments_do_not_read_keeps_its_pid_and_parent() {
        let kinfo = kinfo_of(8, 7, 501, b"sl\xffep", 2);
        for procargs in [None, Some(&[][..])] {
            let entry = listed_from_kinfo(&kinfo, procargs, None, NOV_4).unwrap();
            assert_eq!((entry.pid, entry.ppid), (8, 7));
            assert_eq!(entry.command, "(sl\u{fffd}ep)");
            assert_eq!(entry.cpu_ms, None);
        }
        let zombie = kinfo_of(8, 7, 501, b"sleep", 5);
        let entry = listed_from_kinfo(&zombie, None, None, NOV_4).unwrap();
        assert_eq!((entry.pid, entry.ppid), (8, 7));
        assert_eq!(entry.command, "<defunct>");
        assert_eq!(entry.cpu_ms, Some(0));
        assert_eq!(listed_from_kinfo(&[], None, None, NOV_4), None);
    }

    #[test]
    fn a_full_length_macos_name_ends_at_the_end_of_its_field() {
        let kinfo = kinfo_of(8, 7, 501, &[b'a'; KINFO_COMM_SIZE], 2);
        let entry = listed_from_kinfo(&kinfo, None, None, NOV_4).unwrap();
        assert_eq!(entry.command, format!("({})", "a".repeat(KINFO_COMM_SIZE)));
    }

    #[test]
    fn the_real_uid_is_read_from_the_struct_and_from_the_status_text() {
        assert_eq!(
            real_uid_from_kinfo(&kinfo_of(8, 7, 501, b"x", 2)),
            Some(501)
        );
        assert_eq!(real_uid_from_kinfo(&[]), None);
        assert_eq!(
            real_uid_from_status("Name:\tx\nPid:\t8\nUid:\t1000\t0\t0\t0\nGid:\t5\t5\t5\t5\n"),
            Some(1000)
        );
        assert_eq!(real_uid_from_status("Name:\tx\n"), None);
    }

    #[test]
    fn a_linux_entry_reads_arguments_that_are_not_utf8_and_keeps_one_that_has_none() {
        let stat = proc_text("a) b", 'S');
        let read = |cmdline, stat: &str| {
            listed_from_proc(stat, PROC_STAT, 100, cmdline, 1_793_000_000 + 1_300).unwrap()
        };
        let entry = read(Some(&b"x\0b\xff\0two\n  lines\0"[..]), &stat);
        assert_eq!((entry.pid, entry.ppid), (4242, 17));
        assert_eq!(entry.elapsed_secs, 66);
        assert_eq!(entry.command, "x b\u{fffd} two lines");
        for cmdline in [None, Some(&b""[..])] {
            let entry = read(cmdline, &stat);
            assert_eq!((entry.pid, entry.ppid), (4242, 17));
            assert_eq!(entry.command, "[a) b]");
        }
        assert_eq!(read(None, &proc_text("sh", 'Z')).command, "[sh] <defunct>");
        assert_eq!(listed_from_proc("", PROC_STAT, 100, None, 0), None);
    }

    #[test]
    fn a_refused_list_of_pids_is_not_an_empty_one() {
        assert_eq!(pid_count(3), Some(3));
        assert_eq!(pid_count(0), None);
        assert_eq!(pid_count(-1), None);
    }

    #[test]
    fn mach_units_become_milliseconds_by_the_timebase() {
        // Apple silicon: 125/3 nanoseconds a unit, so 24 million units are
        // one second.
        assert_eq!(cpu_ms_from_mach(18_000_000, 6_000_000, 125, 3), Some(1_000));
        // Intel: a unit is a nanosecond.
        assert_eq!(cpu_ms_from_mach(1_500_000, 500_000, 1, 1), Some(2));
        assert_eq!(cpu_ms_from_mach(999_999, 0, 1, 1), Some(0));
        // A long-running process does not overflow on the way.
        assert_eq!(
            cpu_ms_from_mach(u64::MAX, 0, 125, 3),
            Some(768_614_336_404_564)
        );
        assert_eq!(cpu_ms_from_mach(1, 1, 1, 0), None);
    }

    #[test]
    fn clock_ticks_are_read_past_a_name_with_a_space_and_a_parenthesis() {
        assert_eq!(
            proc_stat_cpu_ticks("42 (a b) c) S 1 42 42 0 -1 4194304 90 0 0 0 7 5 0 0 20 0 1 0"),
            Some(12)
        );
        assert_eq!(proc_stat_cpu_ticks("42 (x) S 1 2"), None);
        assert_eq!(proc_stat_cpu_ticks("no command"), None);
    }

    #[test]
    fn the_age_is_the_time_since_the_start_and_never_below_nothing() {
        assert_eq!(elapsed_secs(NOV_4 + 65, NOV_4), 65);
        assert_eq!(elapsed_secs(NOV_4, NOV_4), 0);
        assert_eq!(elapsed_secs(NOV_4 - 1, NOV_4), 0);
        assert_eq!(
            elapsed_secs(i64::MAX, i64::MIN),
            u64::try_from(i64::MAX).unwrap()
        );
    }

    #[test]
    fn the_list_holds_this_process_with_its_parent_and_a_cpu_time_that_does_not_fall() {
        let pid = std::process::id();
        let now = read_process(pid).unwrap().started_secs + 65;
        let own = || {
            list_user_processes(now)
                .unwrap()
                .into_iter()
                .find(|process| process.pid == pid)
                .unwrap()
        };
        let first = own();
        assert_eq!(first.ppid, std::os::unix::process::parent_id());
        assert_eq!(first.elapsed_secs, 65);
        let first_argument = std::env::args().next().unwrap();
        assert!(first.command.starts_with(&first_argument), "{first:?}");
        let second = own();
        assert!(second.cpu_ms.unwrap() >= first.cpu_ms.unwrap());
    }
}
