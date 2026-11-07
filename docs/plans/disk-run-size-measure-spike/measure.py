#!/usr/bin/env python3
"""Measurements for docs/plans/disk-run-size-measure-spike.md.

Only reads and removes under this run's worktree and TMPDIR.
  measure.py cycle N [--rm-estimate SECONDS]   one round: empty target -> build -> (2) and (3)
  measure.py clone [--repeat N]                (4) in TMPDIR
  measure.py inspect PATH                      (5) links and clones under PATH
Results are appended as CSV next to this script.
"""
import csv, json, ctypes, os, shutil, stat, struct, subprocess, sys, threading, time
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
TMP = os.environ["TMPDIR"]
WORKTREE = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip()
TARGET = os.path.join(WORKTREE, "target")


def tree(root):
    """The rule of tree_size in src/infrastructure/run_files.rs: lstat, the root
    included, directories counted, links not followed, (st_dev, st_ino) once."""
    st = os.lstat(root)
    if not stat.S_ISDIR(st.st_mode):
        raise ValueError("tree root must be a directory, not a link")
    seen = {(st.st_dev, st.st_ino)}
    total, files = st.st_blocks * 512, 0
    pending = [root]
    while pending:
        path = pending.pop()
        try:
            it = os.scandir(path)
        except (FileNotFoundError, NotADirectoryError):
            continue
        with it:
            for entry in it:
                try:
                    st = entry.stat(follow_symlinks=False)
                except FileNotFoundError:
                    continue
                if stat.S_ISDIR(st.st_mode):
                    pending.append(entry.path)
                else:
                    files += 1
                key = (st.st_dev, st.st_ino)
                if key not in seen:
                    seen.add(key)
                    total += st.st_blocks * 512
    return total, files


def naive(root):
    """Comparison only: st_blocks of every entry, duplicates not removed."""
    total = os.lstat(root).st_blocks * 512
    pending = [root]
    while pending:
        with os.scandir(pending.pop()) as it:
            for entry in it:
                st = entry.stat(follow_symlinks=False)
                total += st.st_blocks * 512
                if stat.S_ISDIR(st.st_mode):
                    pending.append(entry.path)
    return total


def du(root):
    out = subprocess.run(["du", "-sk", root], capture_output=True, text=True, check=True).stdout
    return int(out.split()[0]) * 1024


def free():
    s = os.statvfs(WORKTREE)
    return s.f_bavail * s.f_frsize


def load1():
    return os.getloadavg()[0]


def snapshots():
    out = []
    for volume in ("/", "/System/Volumes/Data"):
        text = subprocess.run(["tmutil", "listlocalsnapshots", volume], capture_output=True, text=True, check=True).stdout
        out += [line for line in text.splitlines() if "com.apple" in line]
    return sorted(set(out))


def append(name, header, row):
    path = os.path.join(HERE, name)
    fresh = not os.path.exists(path)
    with open(path, "a", newline="") as f:
        w = csv.writer(f, lineterminator="\n")
        if fresh:
            w.writerow(header)
        w.writerow(row)


class Attr(ctypes.Structure):
    _fields_ = [("count", ctypes.c_ushort), ("reserved", ctypes.c_ushort), ("common", ctypes.c_uint),
                ("vol", ctypes.c_uint), ("dir", ctypes.c_uint), ("file", ctypes.c_uint), ("fork", ctypes.c_uint)]


LIBC = ctypes.CDLL(None, use_errno=True)
PRIVATESIZE, CLONEID, CLONE_REFCNT = 0x8, 0x100, 0x1000
FSOPT_NOFOLLOW, FSOPT_ATTR_CMN_EXTENDED = 0x1, 0x20


def clone_info(path):
    """(private bytes, clone id, clone refcount) from getattrlist(2), None if unreadable."""
    attr = Attr(5, 0, 0, 0, 0, 0, PRIVATESIZE | CLONEID | CLONE_REFCNT)
    buf = ctypes.create_string_buffer(64)
    if LIBC.getattrlist(path.encode(), ctypes.byref(attr), buf, 64, FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED) != 0:
        return None
    _, private, clone_id, refcnt = struct.unpack_from("<IqQI", buf.raw)
    return private, clone_id, refcnt


def inspect(root, cycle="-"):
    """Links and clones under root: what stat alone cannot tell apart."""
    count, blocks, nlink, sample = Counter(), {}, {}, {}
    naive_bytes = os.lstat(root).st_blocks * 512
    symlinks_out, symlinks, clone_groups, shared, cloned_files, unread = [], 0, {}, 0, 0, 0
    real_root = os.path.realpath(root)
    pending = [root]
    while pending:
        with os.scandir(pending.pop()) as it:
            for entry in it:
                st = entry.stat(follow_symlinks=False)
                naive_bytes += st.st_blocks * 512
                if stat.S_ISDIR(st.st_mode):
                    pending.append(entry.path)
                    continue
                if stat.S_ISLNK(st.st_mode):
                    symlinks += 1
                    real = os.path.realpath(entry.path)
                    if not real.startswith(real_root + os.sep):
                        symlinks_out.append((entry.path, real))
                    continue
                key = (st.st_dev, st.st_ino)
                count[key] += 1
                blocks[key], nlink[key] = st.st_blocks * 512, st.st_nlink
                sample.setdefault(key, entry.path)
    for key, path in sample.items():
        info = clone_info(path)
        if info is None:
            unread += 1
            continue
        private, clone_id, refcnt = info
        if refcnt > 1:
            cloned_files += 1
            over = max(0, blocks[key] - private)
            shared += over
            clone_groups.setdefault(clone_id, []).append((blocks[key], path))
    multi = sum(count[k] * blocks[k] for k in count if count[k] >= 2)
    outside = [k for k in count if nlink[k] > count[k]]
    inside_groups = {c: g for c, g in clone_groups.items() if len(g) >= 2}
    inside_over = sum(sum(b for b, _ in g) - max(b for b, _ in g) for g in inside_groups.values())
    biggest = sorted(((sum(b for b, _ in g), [p.replace(root, "") for _, p in g][:2]) for g in inside_groups.values()), reverse=True)[:5]
    result = dict(
        naive=naive_bytes, multi_link_bytes=multi, multi_ratio=multi / naive_bytes if naive_bytes else 0,
        multi_inodes=sum(1 for k in count if count[k] >= 2), inodes=len(count),
        links_outside_inodes=len(outside), links_outside_bytes=sum(blocks[k] for k in outside),
        symlinks=symlinks, symlinks_outside=len(symlinks_out), symlinks_outside_sample=symlinks_out[:5],
        clone_files=cloned_files, clone_shared_bytes=shared, clone_groups_inside=len(inside_groups),
        clone_inside_over_bytes=inside_over, clone_biggest=biggest, clone_unreadable=unread,
    )
    print("inspect", root, result, flush=True)
    append("inspect.csv", ["cycle", "root", "result"], [cycle, root.replace(WORKTREE, "<worktree>").replace(TMP, "<tmp>"), repr(result).replace(WORKTREE, "<worktree>").replace(TMP, "<tmp>")])
    return result


def sampler(cycle, stop, started):
    """(1): every 30 seconds while the build runs."""
    while True:
        t = time.time()
        if os.path.exists(TARGET):
            size, files = tree(TARGET)
            took = time.time() - t
        else:
            size, files, took = 0, 0, 0.0
        append("growth.csv", ["cycle", "elapsed_s", "tree_bytes", "files", "walk_s", "free_bytes", "load1"],
               [cycle, round(t - started), size, files, round(took, 2), free(), round(load1(), 2)])
        if stop.wait(max(0, 30 - (time.time() - t))):
            return


def run(label, cmd):
    t = time.time()
    code = subprocess.run(cmd, cwd=WORKTREE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode
    append("builds.csv", ["label", "command", "exit", "wall_s"],
           [label, " ".join(cmd).replace(TMP, "<tmp>"), code, round(time.time() - t, 2)])
    print(f"{label}: exit {code} in {time.time() - t:.0f}s", flush=True)
    return code, time.time() - t


def cycle(n, rm_estimate):
    if os.path.exists(TARGET):
        shutil.rmtree(TARGET)
    before = snapshots()
    append("snapshots.csv", ["cycle", "phase", "snapshots"], [n, "before_build", json.dumps(before)])
    started = time.time()
    stop = threading.Event()
    th = threading.Thread(target=sampler, args=(n, stop, started))
    th.start()
    brief = ["sh", "scripts/cargo-brief.sh", "--log"]
    run("clippy", brief + [f"{TMP}/clippy-{n}.log", "cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"])
    run("test --no-run", brief + [f"{TMP}/lib-{n}.log", "cargo", "test", "--locked", "--lib", "--no-run"])
    stop.set()
    th.join()
    t = time.time()
    size, files = tree(TARGET)
    append("growth.csv", ["cycle", "elapsed_s", "tree_bytes", "files", "walk_s", "free_bytes", "load1"],
           [n, round(t - started), size, files, round(time.time() - t, 2), free(), round(load1(), 2)])

    # (2) three counts of three roots, three rounds.
    roots = {"worktree": WORKTREE, "target": TARGET, "tmpdir": TMP}
    methods = {"tree": lambda p: tree(p)[0], "du": du, "naive_comparison_only": naive}
    run_time = 0.0
    for round_no in (1, 2, 3):
        round_time = 0.0
        for name, root in roots.items():
            for method, fn in methods.items():
                l = load1()
                t = time.time()
                value = fn(root)
                took = time.time() - t
                append("counts.csv", ["cycle", "root", "method", "round", "bytes", "wall_s", "load1"],
                       [n, name, method, round_no, value, round(took, 3), round(l, 2)])
                if method == "tree" and name in ("worktree", "tmpdir"):
                    round_time += took
        run_time = max(run_time, round_time)
    info = inspect(TARGET, n)

    # (3) noise over W, then the removal.
    window = rm_estimate + 30
    f0 = free()
    low, high = f0, f0
    end = time.time() + window
    while time.time() < end:
        time.sleep(5)
        now = free()
        low, high = min(low, now), max(high, now)
    f1 = free()
    noise = abs(f1 - f0)
    size, _ = tree(TARGET)
    du_target = du(TARGET)
    before_rm = free()
    t = time.time()
    shutil.rmtree(TARGET)
    rm_s = time.time() - t
    f_now = free()
    time.sleep(30)
    f_later = free()
    delta = max(f_now - before_rm, f_later - before_rm)
    after = snapshots()
    append("snapshots.csv", ["cycle", "phase", "snapshots"], [n, "after_removal", json.dumps(after)])
    new_snapshots = sorted(set(after) - set(before))
    comparable = noise <= size * 0.05 and not new_snapshots
    e = (size - delta) / size
    append("cycles.csv",
           ["cycle", "tree", "du", "du_diff", "window_s", "noise", "noise_range", "delta_free_now", "delta_free_30s",
            "delta_free_rm", "e_target", "rm_s", "new_snapshots", "comparable", "max_measure_s", "multi_ratio"],
           [n, size, du_target, round(abs(size - du_target) / du_target, 5), round(window), noise, high - low,
            f_now - before_rm, f_later - before_rm, delta, round(e, 4), round(rm_s, 1), len(new_snapshots), comparable,
            round(run_time, 2), round(info["multi_ratio"], 4)])
    print(f"cycle {n}: tree={size} du={du_target} noise={noise} delta={delta} E={e:.4f} rm={rm_s:.1f}s "
          f"comparable={comparable} measure={run_time:.2f}s total={time.time() - started:.0f}s", flush=True)


def clone(repeat):
    root = os.path.join(TMP, "clone-exp")
    os.makedirs(root, exist_ok=True)
    src, dst = os.path.join(root, "source.bin"), os.path.join(root, "clone.bin")
    before_snapshots = snapshots()
    if not os.path.exists(src):
        subprocess.check_call(["dd", "if=/dev/urandom", f"of={src}", "bs=1m", "count=2048"], stderr=subprocess.DEVNULL)
    time.sleep(30)  # let the write settle before the first window
    for n in range(1, repeat + 1):
        f0 = free()
        time.sleep(35)
        f1 = free()
        noise = abs(f1 - f0)
        tree0, du0, free0 = tree(TMP)[0], du(TMP), free()
        code = subprocess.run(["cp", "-c", src, dst]).returncode
        tree1, du1, free1 = tree(TMP)[0], du(TMP), free()
        info = clone_info(dst)
        after_snapshots = snapshots()
        dtree, ddu, dfree = tree1 - tree0, du1 - du0, free0 - free1
        e = (dtree - dfree) / dtree if dtree else None
        append("clone.csv", ["repeat", "cp_exit", "delta_tree", "delta_du", "delta_free", "noise", "e_clone",
                             "clone_private_bytes", "clone_refcnt", "new_snapshots", "comparable"],
               [n, code, dtree, ddu, dfree, noise, None if e is None else round(e, 4),
                info and info[0], info and info[2], len(set(after_snapshots) - set(before_snapshots)),
                code == 0 and dtree > 0 and noise <= dtree * 0.05 and not (set(after_snapshots) - set(before_snapshots))])
        print(f"clone {n}: exit={code} dtree={dtree} ddu={ddu} dfree={dfree} noise={noise} E={e} info={info}", flush=True)
        os.remove(dst)
    shutil.rmtree(root)


if __name__ == "__main__":
    action = sys.argv[1]
    if action == "cycle":
        estimate = int(sys.argv[4]) if len(sys.argv) > 4 else 30
        cycle(int(sys.argv[2]), estimate)
    elif action == "clone":
        clone(int(sys.argv[3]) if len(sys.argv) > 3 else 3)
    elif action == "inspect":
        inspect(sys.argv[2])
    print("DONE", flush=True)
