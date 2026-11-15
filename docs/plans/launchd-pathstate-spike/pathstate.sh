#!/bin/sh
# launchd の KeepAlive PathState（自分の plist の path）の振る舞いを使い捨ての
# LaunchAgent（gui/$UID、label は com.dagq.spike-pathstate.<乱数>.<名>）で確かめる。
# plist と job は mktemp -d の一時の directory に置き、~/Library/LaunchAgents には置かない。
# 使い方: sh pathstate.sh <結果を置く directory>
set -u
out=${1:?usage: pathstate.sh OUTDIR}
mkdir -p "$out"
here=$(cd "$(dirname "$0")" && pwd)
uid=$(id -u)
dom="gui/$uid"
rand=$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')
base="com.dagq.spike-pathstate.$rand"
d=$(mktemp -d "${TMPDIR:-/tmp}/pathstate.XXXXXX")
cp "$here/job.sh" "$d/job.sh"
names="a b c d e"
log="$out/run.log"
: > "$log"
started_at=${LOG_START:-}
[ -n "$started_at" ] || started_at=$(date '+%Y-%m-%d %H:%M:%S')

say() { echo "$(date -u +%H:%M:%SZ) $*" | tee -a "$log"; }
label() { echo "$base.$1"; }

cleanup() {
  say "cleanup"
  for n in $names; do rm -f "$d/$n.termhold"; done
  for n in $names; do
    launchctl bootout "$dom/$(label "$n")" >/dev/null 2>&1
  done
  sleep 2
  for n in $names; do
    if launchctl print "$dom/$(label "$n")" >/dev/null 2>&1; then
      say "LEFT $(label "$n")"
    else
      say "gone $(label "$n"): $(launchctl print "$dom/$(label "$n")" 2>&1 | head -1)"
    fi
  done
  rm -rf "$d"
  say "removed $d: $( [ -e "$d" ] && echo no || echo yes )"
  launchctl list | grep -c "$base" | sed 's/^/labels left in launchctl list: /' | tee -a "$log"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# plist: $1 名、$2 KeepAlive の dict に足す行（O8 の SuccessfulExit）
plist() {
  n=$1
  extra=${2:-}
  p="$d/$n.plist"
  cat > "$p" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$(label "$n")</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>$d/job.sh</string>
		<string>$d</string>
		<string>$n</string>
	</array>
	<key>WorkingDirectory</key>
	<string>$d</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
	<key>KeepAlive</key>
	<dict>
		<key>PathState</key>
		<dict>
			<key>$p</key>
			<true/>
		</dict>$extra
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>ExitTimeOut</key>
	<integer>86400</integer>
	<key>StandardOutPath</key>
	<string>$d/$n.log</string>
	<key>StandardErrorPath</key>
	<string>$d/$n.log</string>
</dict>
</plist>
PLIST
  cp "$p" "$out/$n.plist"
}

starts() { grep -c '^start' "$d/$1.count" 2>/dev/null || echo 0; }
lastpid() { grep '^start' "$d/$1.count" | tail -1 | awk '{print $3}'; }
pr() {
  say "print $(label "$1"): $(launchctl print "$dom/$(label "$1")" 2>&1 | grep -E '^[[:space:]]*(state|pid|runs|last exit code|last terminating signal|exit timeout) =' | sed 's/^[[:space:]]*//' | tr '\n' ';')"
}
# $1 名、$2 待つ回数、$3 秒の上限。回数に届いた秒数を返す（届かなければ上限）
wait_starts() {
  i=0
  while [ "$i" -lt "$3" ]; do
    [ "$(starts "$1")" -ge "$2" ] && { echo "$i"; return 0; }
    sleep 1; i=$((i + 1))
  done
  echo "$i"; return 1
}
boot() {
  plist "$@"
  msg=$(launchctl bootstrap "$dom" "$d/$1.plist" 2>&1); rc=$?
  say "bootstrap $(label "$1") rc=$rc $msg"
  [ "$rc" -eq 0 ] || exit 2
  w=$(wait_starts "$1" 1 20); say "$1 first start after ${w}s starts=$(starts "$1") pid=$(lastpid "$1")"
}
alive() { kill -0 "$1" 2>/dev/null && echo alive || echo gone; }

say "base=$base dir=$d uid=$uid only=${ONLY:-all}"
# ONLY=o5 は O5 の直と対照だけを流す（通しの 1 回目が host の sleep で O5 の対照に届かなかったときの 2 回目）
if [ "${ONLY:-}" != o5 ]; then
boot a
boot b
boot e "
		<key>SuccessfulExit</key>
		<false/>"
sleep 11  # ThrottleInterval の 10 秒を越えてから終わらせる

# O1: plist がある間の終了コード 0
echo 0 > "$d/a.cmd"; w=$(wait_starts a 2 40); say "O1 a exit0 -> starts=$(starts a) after ${w}s pid=$(lastpid a)"; pr a
sleep 11
# O2: plist がある間の終了コード 1
echo 1 > "$d/a.cmd"; w=$(wait_starts a 3 40); say "O2 a exit1 -> starts=$(starts a) after ${w}s pid=$(lastpid a)"; pr a
sleep 11
# O3: 外からの kill -KILL
p=$(lastpid a); kill -KILL "$p"; say "O3 kill -KILL $p rc=$?"; w=$(wait_starts a 4 40); say "O3 a -> starts=$(starts a) after ${w}s pid=$(lastpid a)"; pr a
sleep 11

# O4（a は 0、b は 1）と O8（e は SuccessfulExit=false で 1）: plist を消してから終わらせる
for n in a b e; do rm -f "$d/$n.plist"; done
say "O4/O8 removed plists of a b e"
sleep 5
for n in a b e; do say "$n after rm: starts=$(starts "$n") pid=$(lastpid "$n") $(alive "$(lastpid "$n")")"; pr "$n"; done
sa=$(starts a); sb=$(starts b); se=$(starts e)
echo 0 > "$d/a.cmd"; echo 1 > "$d/b.cmd"; echo 1 > "$d/e.cmd"
say "O4/O8 asked a=0 b=1 e=1"
sleep 65
say "O4 a exit0 without plist: starts $sa -> $(starts a)"; pr a
say "O4 b exit1 without plist: starts $sb -> $(starts b)"; pr b
say "O8 e exit1 without plist (SuccessfulExit=false): starts $se -> $(starts e)"; pr e

# O7: a（plist 無し・job 止まり）に同じ path へ plist を書き戻す
sa=$(starts a)
plist a; say "O7 wrote back $d/a.plist"
w=$(wait_starts a $((sa + 1)) 40); say "O7 a after write-back: starts $sa -> $(starts a) (${w}s)"; pr a

# O6: plist の無い load の agent（b）を bootout
msg=$(launchctl bootout "$dom/$(label b)" 2>&1); say "O6 bootout b rc=$? $msg"
say "O6 print b: $(launchctl print "$dom/$(label b)" 2>&1 | head -1)"

fi

# O5 直: plist を消した load の c に kill -TERM を直に送る
boot c
sleep 11
touch "$d/c.termhold"; rm -f "$d/c.plist"
p=$(lastpid c); t0=$(date +%s); kill -TERM "$p"; say "O5 direct kill -TERM $p rc=$? t0=$t0"
while kill -0 "$p" 2>/dev/null && [ $(( $(date +%s) - t0 )) -lt 150 ]; do sleep 1; done
t1=$(date +%s); say "O5 direct c pid $p $(alive "$p") after $((t1 - t0))s"; pr c
cat "$d/c.count" | sed 's/^/c.count: /' | tee -a "$log"
sleep 15; say "O5 direct c starts after end: $(starts c)"
msg=$(launchctl bootout "$dom/$(label c)" 2>&1); say "O6 bootout c rc=$? $msg"
say "O6 print c: $(launchctl print "$dom/$(label c)" 2>&1 | head -1)"

# O5 対照: plist のある d を launchctl bootout
boot d
sleep 11
touch "$d/d.termhold"
p=$(lastpid d); t0=$(date +%s)
msg=$(launchctl bootout "$dom/$(label d)" 2>&1); rc=$?; tb=$(date +%s)
say "O5 control bootout d rc=$rc returned after $((tb - t0))s $msg"
while kill -0 "$p" 2>/dev/null && [ $(( $(date +%s) - t0 )) -lt 150 ]; do sleep 1; done
t1=$(date +%s); say "O5 control d pid $p $(alive "$p") after $((t1 - t0))s"
cat "$d/d.count" | sed 's/^/d.count: /' | tee -a "$log"

for n in a b c d e; do [ -f "$d/$n.count" ] && cp "$d/$n.count" "$out/$n.count"; done
sleep 3
log show --info --debug --style compact --start "$started_at" --predicate "eventMessage CONTAINS \"$base\"" > "$out/launchd.log" 2>&1
say "launchd log lines: $(wc -l < "$out/launchd.log")"
