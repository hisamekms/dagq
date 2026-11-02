---
id: plan-ledger-cut-at-position-spike
type: plan
title: スパイク：台帳の係の queue 行・待ちの行・supervisor の一生を、store の行と範囲の決まった読みから位置 at の時点に切って作り直せるか
status: completed
created: 2026-11-02
owners:
  - hisamekms
tags:
  - planning
  - measurement
related:
  - plan-supervisor-startup-memory-spike
---

# スパイク：queue 行・待ち・一生を位置 at に切る

問いは「台帳の係の queue の帳簿（`QueueBook::from_heads`、`src/domain/ledger/queue.rs`）と待ち・一生の帳簿（`domain::pre_claim::rows::books` が返す `Lives` と `PreClaimBook`、`src/domain/pre_claim/rows.rs`）を、store の queue 行（各行は書いた pass の位置 P の時点のもの）と、範囲の決まった対象別・id 指定の読みだけから、位置 at（at ≤ P）の時点に切って作り直せるか」。
種類ごとに試作の切り方（`cut`）を書き、P で書いた行を at に切った行が、at までの event の全件で pass が書く行（`queue_rows` の行）と同じになるかを、at を履歴のすべての位置に取って比べた。

判定は種類ごとに分かれる（下の「判定」）。
`landing_slot` の 1 種類だけが範囲の決まった読みで at に正確には切れず（does_not_hold）、それ以外は行の頭と records だけか、決まった読み（1 つは store に足す読み）を足せば切れる（holds）。

## 環境

- 対象の commit: `7fe35c1e7726f93ecda1e3a9f2636877bc7d05b6`（この run の base）。
- host: Apple M1（MacBookAir10,1）、メモリ 16 GiB（`hw.memsize` 17179869184 byte）、macOS 27.2（26B123）。
- 道具: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`。
- 実行の経路: dagq の worker run（task 3979、run `bc0ed4b1-250f-4314-a196-b4611819f29a`）。provider は Claude（Claude Code 2.1.316 の `sdk-cli` の entrypoint、model は Opus 5.5）で、`CLAUDE_CODE_SESSION_ATTENDED=0` の headless の background の worker run の中で打った。人の terminal・planner・subagent の経路ではない。
- 本番の queue・store には触れていない。試作は domain の unit test の中だけで、store は event の列から読みを数える test の偽物（`Store`）である。

## 打ったもの

試作の test は `src/domain/ledger/queue/cut_spike.rs`（`#[cfg(test)]` の module、`queue.rs` の末尾で宣言）に残した。

```sh
cargo test --lib cut_spike -- --nocapture
cargo test --lib -- domain::ledger::queue:: domain::pre_claim::rows::tests domain::ledger::builder::tests application::supervise::observation::ledger::tests
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
```

2 つ目は 109 件が通った（試作の 7 件と、既存の `domain::ledger::queue::tests`・`domain::pre_claim::rows::tests`・`domain::ledger::builder::tests`・`application::supervise::observation::ledger::tests`。既存の test は変えていない）。

試作の test:

| test | 確かめること |
|---|---|
| `spike_cut_rows_match_the_rows_written_at_the_position` | `landing_slot` 以外の全種類で、P の行を at に切った行が at の pass が書く行と同じ（`QueueHead` で比べる）。`landing_slot` は through（`inputs_through.events`）の外では同じ。3 つの履歴 × 2 つの時刻 × すべての位置 |
| `spike_landing_slot_through_needs_a_range_read` | `landing_slot` は P の through のままだと at を越える位置があり、範囲を走査すると同じになる |
| `spike_reads_per_cut_do_not_grow_with_settled_rows` | 1 行の切りの読みの数を、確定済みの行だけを増やした大小 2 つの履歴で比べる |
| `spike_since_cut_is_no_later_than_the_since_at_the_position` | P の task 行から切った `Since` が at の正確な `Since` より後にならず、待ちの行の選びが上位集合になる |
| `spike_wait_books_of_rows_cut_are_those_of_the_rows_at_the_position` | 切った行と切った `Since` から `rows::books` で作った `Lives` と `PreClaimBook` が、at の行から作った帳簿の待ちをすべて同じ値で持ち、各 supervisor の一生の終わりが同じ |
| `spike_book_of_rows_cut_goes_on_as_the_fold_of_every_event` | 切った行（`landing_slot` の through は P のまま）から `QueueBook::from_heads` で作り直し、(at, P] の event を与えると、全件の fold の行と同じになる |
| `spike_landing_slot_through_during_the_replay` | 上と同じ作り直しで (at, P] を 1 event ずつ再生し、各段の `landing_slot` の行を、その段までの event の全件の fold の行と比べる。違いは through だけで、再生した event が attempt に届くまでは fold より先（P の値）、届いた後は fold と同じ。`two_late`（終わりの後に終わり以前の時刻の event A・B が来る履歴）で through が B から A に戻る段がある |

履歴は 3 つ: この spike の履歴（`spike_log`。全種類の行、attempt の終わりの後に来る終わり以前の時刻の event、supervisor の停止で終わる hold、沈黙で終わり後の event で開き直す slots full、claim で終わる deferral、次の hold で置き換わる hold、置き換わる stream、終わる alert、解決する finding）、testkit の `queue_across` と `waits_across`。
時刻は最後の event の 1 秒後と、すべての沈黙を越える時刻（`SILENT_AFTER_MS` の 10 倍後）の 2 つ。
at は各履歴のすべての event の id で、attempt・待ち・stream・alert の始まりと終わりの直前・直後、沈黙の閾値の前後、claim を跨ぐ待ちを含む。

## 試作の切り方

`cut` は P の行 1 行ずつを、行の頭（`QueueHead` が持つ欄と records の id）と下の読みだけで at の行にする。
key が at より後の行は at には無い（落とす）。
読みは 2 通り（`Way`）で試した。

- `Bounded`: supervisor を名指す at 以前の最後の event を、store に足す id 指定の読み（下の「足す読み」）で 1 回で読む。`landing_slot` の through は P の値のまま。
- `Range`: その 2 つを id の範囲の走査で求める（at から遡る、終わりの event から at まで）。

## 判定

| 種類 | 判定 | 根拠（コードの位置と試作の test） | 足す読み |
|---|---|---|---|
| 一つの event の行 | holds | 行は自分の event だけを records に持ち、`ended_at` は始まりと同じ（`QueueBook::add` の `!taken` の枝）。key ≤ at ならそのまま。`spike_cut_rows_match_the_rows_written_at_the_position` | 無し（行の頭だけ） |
| `live_alert` | holds | records は始まりと `live_alert_ended` だけ（`QueueBook::alert`）。終わりの record が at より後なら開いた alert（`ended_at`・`outcome` 無し、through は始まり）。detail は始まりの payload で at に依らない。同上の test | 無し（行の頭だけ） |
| `claim_deferral` | holds | 終わりは records に無い `run_claimed` でも、その id は行の `inputs_through.events`（`QueueBook::add` の `ended_by` と `PreClaimInterval::reach`）。それが at 以前なら P の行のまま、後なら開いた待ち（detail の終わりを消し、through は始まり）。同上の test | 無し（行の頭だけ） |
| `landing_hold` | holds | `claim_deferral` と同じ形（次の hold で置き換わる終わりも `inputs_through.events`）。同上の test | 無し（行の頭だけ） |
| `claim_hold` | holds | `claim_deferral` と同じ形。ただし開いた supervisor の停止で終わった hold は `inputs_through.supervisor` に「その supervisor を名指す最後の event」を持つ（`QueueBook::row` の `life`）ので、at 以前の最後の event が要る。同上の test | 停止で終わった hold だけ、足す読み 1 回 |
| `finding` | holds | records は finding の全 record。at より後の record があれば、at 以前の最後の record の本体から `outcome`・`ended_at`・detail、最初の task の record から `task_id` を作る（`QueueBook::finding`・`fill`）。同上の test | その行の records の本体（`LedgerStore::ledger_row(Queue, key)`、行の records の数まで）。最後の record と task の record だけなら id 指定の読みでもよい |
| `live_alert_stream` | holds（足す読みで） | 置き換わり（`replaced`）の終わりは次の baseline で、その id は `inputs_through.events`。それが at 以前なら P の行のまま。後なら at の時点で開いた stream: records を at で切り、`version` は at 以前の最後の record の値（本体が要る）、through と一生の終わりは supervisor を名指す at 以前の最後の event と停止で決まる（`QueueBook::row` の `reach`・`fill`）。同上の test | records の本体（`ledger_row`）と足す読み 1 回 |
| `slots_full` | holds（足す読みで） | `freed` の終わりは自分の record で、その id が at 以前なら P の行のまま。後か解かれていないなら、at の時点の `reach` は supervisor を名指す at 以前の最後の event（`PreClaimBook::reach_slots_full`）、一生による終わりはその event の時刻と at 以前の停止で決まる（`at_life`）。同上の test と `spike_wait_books_of_rows_cut_are_those_of_the_rows_at_the_position` | 足す読み 1 回 |
| `landing_slot` | does_not_hold | 終わったかは範囲の決まった読みで決まる: 次の slot の key（行の頭）が at 以前か、自分の run の at 以前の event（`LedgerStore::ledger_run_events`）に終わり（`Attempts::add`）があるか。だが終わった attempt の through は「時刻が終わり以前の、at 以前の最大の id」で、P の through が at を越えるとき（(at, P] に終わり以前の時刻の event がある）、正確な値は終わりの event から at までの走査でしか出ない（`Throughs::reach` が `Attempts::ended_since(時刻)` で進めるため）。id の範囲と時刻のどちらの索引でもこの最大は決まった数の行では引けない。`spike_landing_slot_through_needs_a_range_read`（P のままでは key 9 の行が at = 10〜15 で違う）と下の読みの数 | （代替は「推奨」） |
| supervisor の一生（`rows::told` の `last_id`・`ended_ms` と stops） | holds（足す読みで） | `last_id` は「at 以前で S を名指す最後の event」、`ended_ms` はその event の時刻（`rows::books` と同じく時計が戻らない前提）、stops は queue 行の records の停止を at で切る（`LedgerStore::ledger_supervisor_stops` の結果を id ≤ at に絞る）。これで `Lives::go_on` と `Lives::add` が at の帳簿と同じ終わりを出す。`spike_wait_books_of_rows_cut_are_those_of_the_rows_at_the_position` | 足す読み 1 回（読む supervisor は `rows::told` が挙げる、行の届く supervisor だけ） |
| `Since` | holds（下限として） | at の正確な `Since` は at の時点の task 行の確定に依るが、P の task 行のうち `inputs_from ≤ at` で「P で確定していないか、入力が at を越える」行の最小から作った `Since` は正確な値より後にならない（`spike_since_cut_is_no_later_than_the_since_at_the_position`）。早い `Since` は `rows::holds` が待ちの行を多く選ぶだけで、作った帳簿は正確な `Since` の帳簿の待ちを同じ値で持ち、余分は正確な `Since` より前に終わった待ちだけ（`spike_wait_books_of_rows_cut_are_those_of_the_rows_at_the_position`）。`rows::lacks` で読む確定済みの待ちは帳簿が持つ分だけ減る | `ledger_tasks_open_since` の SQL に `inputs_from <= at` と「`final=0` か入力の最後が at を越える」の条件を足した関数（task 行の走査で、今の `ledger_tasks_open_since` と同じ量。索引と migration は要らない） |

`spike_book_of_rows_cut_goes_on_as_the_fold_of_every_event` は全種類の切った行（`landing_slot` は P の through のまま）から作り直した `QueueBook` に (at, P] の event を与え、全件の fold と同じ行になることを、3 つの履歴 × 2 つの時刻 × すべての at で確かめた。

### 足す読み

supervisor を名指す at 以前の最後の event は、今の port に無い。
`EventStore::latest_events_by_supervisor` は kind を限り、上限 at を取らず、索引の無い `json_extract(payload, '$.supervisor')` で全件を走る（`src/infrastructure/runtime_store/run_log.rs`）。
`MarkLog::events_in_range` の走査は沈黙の長さ（at と最後の event の間の event の数）に比例する。
足す関数の形:

- port: `LedgerStore`（または `EventStore`）に `last_event_naming_supervisor(supervisor: &str, upto: EventId) -> Result<Option<RunEvent>>`。行の届く supervisor ごとに 1 回。
- SQL: `SELECT * FROM run_events WHERE json_extract(payload, '$.supervisor') = ?1 AND id <= ?2 ORDER BY id DESC LIMIT 1`。
- 索引: `CREATE INDEX events_by_supervisor ON run_events(json_extract(payload, '$.supervisor'), id)`。式の索引なので SQL の式は索引と同じ形に書く。migration が要る（既存の event 全件に索引を作る時間と大きさは host で測る）。

## 読みの数の測定

`spike_reads_per_cut_do_not_grow_with_settled_rows` の出力（`--nocapture`）。
小さい履歴は `spike_log(1)`（35 event）、大きい履歴は `spike_log(50)`（329 event）。
増やしたのは確定済みの行だけ（自分の行だけの mark と、claim で終わる別の task の deferral の組）で、最初の attempt の終わりと終わり以前の時刻の event の間と、`s2` の沈黙の中に置いた。
同じ基の位置（増やした event 以外の 29 位置）で全行を 1 行ずつ切った読みの合計と、1 行の切りの最大。

| 種類 | Bounded 小（合計 / 最大） | Bounded 大（合計 / 最大） | Range 小（走査した event の合計 / 最大） | Range 大（同） |
|---|---|---|---|---|
| 一つの event・`live_alert`・`claim_deferral`・`landing_hold` | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| `claim_hold` | 足す読み 15 / 1 | 足す読み 15 / 1 | 138 / 18 | 1020 / 165 |
| `finding` | records 18 / 2 | records 18 / 2 | records 18 / 2 | records 18 / 2 |
| `live_alert_stream` | records 24 / 2、足す読み 28 / 1 | 同じ | 73 / 10（と records 24 / 2） | 367 / 157（同） |
| `slots_full` | 足す読み 28 / 1 | 足す読み 28 / 1 | 335 / 29 | 2687 / 323 |
| `landing_slot` | run の event 24 / 2（注） | run の event 24 / 2（注） | 9 / 6（と run の event 73） | 156 / 153（同） |

Bounded は大小で読みの数が変わらない（test が等しさを assert する）。
ただし足す読みは 1 回の呼び出しを 1 と数えるので、示すのは呼び出しの数が履歴に依らないことで、1 回の費用は索引の引きの費用である（host での測定はしていない）。
注: 偽物の store の `run_events` は run の at 以前の event だけを数える。今の `ledger_run_events(run)` は run の全 event を返すので、実際の読みは run の event の数（上限 at を取る形にしても run 1 つの大きさまで）になる。
Range（足す読みも through の正確な値も id の範囲の走査で求める）は、確定済みの行を増やすと走査が増える（`claim_hold`・`live_alert_stream`・`slots_full`・`landing_slot` の最大がどれも約 9 倍以上）。
Bounded の `landing_slot` は読みが決まっているが、through が at を越える位置で at の行と違う（上の判定）。

## 推奨

- 一部 does_not_hold（`landing_slot` だけ）。task 3977 は「位置を足して切る作り直し」を、下の種類ごとの読みで計画し直せる。
- `landing_slot` の代替: 試作で 2 通りを比べた。
  - (iii) の形（走査で正確な through を求める）は正確だが、読みが終わりの event から at までの event の数に比例し、確定済みの履歴とともに増える（上の表の 9 → 156）。範囲の決まった読みにならない。
  - (i) の形をこの欄にだけ当てる: 終わった attempt の through は P の値のまま持ち（at を越えうる。開いた attempt は at）、終わったか・終わりの時刻・landed は行の頭と `ledger_run_events(run)` を at で切ったもので決める。作り直した後に (at, P] の event を与えれば全件の fold の行と同じになる（`spike_book_of_rows_cut_goes_on_as_the_fold_of_every_event`）。
    再生の途中は全件の fold と同じとは限らない（`spike_landing_slot_through_during_the_replay`）。再生した event がその attempt に届くまでは through が P の値で、その段の fold より先にある。届くと `Throughs::reach` がその event の id で上書きするので、その段から先は fold と同じになる。このため、終わりの後に終わり以前の時刻の event が 2 つ以上あると（A の後に B）、P の through（B）が A の再生で A に戻り、B の再生で B に戻る。途中で書く行は store が持つ P の行より through が小さいことがあり、どの段でも「P の行と同じ」でも「その段の fold と同じ」でもない。違うのは through だけで、終わり・landed・時刻は各段で fold と同じ。
  - **推奨は後者**（through は P の値を持ち、再生を終えて揃える）。根拠: 正確な途中の値には (iii) の走査が要り、読みが履歴とともに増える。後者は読みが決まっていて、再生を終えた後は全件の fold と同じ。途中の through のずれは、P の値で先にあるか、届いた event の id でその段の fold と同じかのどちらかで、終わり以前の時刻の event が (at, P] にあるときだけ起こる。全帳簿をただ 1 つの起動の位置 R で作り直す task 3887 の形では、R から先の event を必ず再生するので成り立つ。3887 の受け入れの「全件の fold と同じ」は、再生を終えた後の行と touched で確かめる形にする。途中で書く `landing_slot` の行の through は全件の fold より先にあることも、store の P の行より後ろに戻ることもある。これを許すか、再生が P に届くまでこの行を書かないかは 3887 で決める（この spike では試作していない）。
- task 3977 に書く種類ごとの読み:
  - 一つの event・`live_alert`・`claim_deferral`・`landing_hold`: `ledger_queue_heads` の行の頭だけ（読みを足さない）。
  - `claim_hold`: 行の頭。開いた supervisor の停止で終わった hold だけ、足す読み `last_event_naming_supervisor(S, at)` 1 回。
  - `finding`: at より後の record を持つ行だけ `ledger_row(Queue, key)` で records の本体（行の records の数まで）。
  - `live_alert_stream`: at より後の record を持つ行だけ `ledger_row(Queue, key)`、置き換わりが at より後か開いた行は足す読み 1 回。
  - `slots_full`: `freed` が at より後か解かれていない行に足す読み 1 回。
  - `landing_slot`: 行の頭（次の slot の key を含む）と、終わりが at より後かを決めるときだけ `ledger_run_events(run)` を at で切ったもの。through は P のまま。
  - supervisor の一生: `rows::told` が挙げる supervisor ごとに足す読み 1 回と、`ledger_supervisor_stops` を id ≤ at に絞ったもの。
  - `Since`: `ledger_tasks_open_since` に at の条件を足した関数（migration 不要）。
  - 足す読みは port の関数 1 つと式の索引の migration 1 つ（上の「足す読み」）。
- 足す読みを避けたいときの代替は (i) をこの帳簿の組（queue・待ち・一生）全体に当てる形: 3 つを queue 行を書き終えた位置 Q で作り直し、(R, Q] の event をこの 3 つにだけ与えない。`QueueBook::add` が受ける `Moved` と `Lives` は同じ Q の帳簿から出るので結び付きは崩れず、Q で作り直して Q の後を与える形は既存の `a_book_made_again_from_the_rows_goes_on_as_the_fold_of_every_event` がそのまま確かめている。ただし task の fold が読む待ちが R の後の位置のものになるので、その間に書く task 行の扱いは別に決める必要がある（この spike では試作していない）。
- 試作で確かめていないこと: 切った queue 行と、切った行から `rows::books` で作った待ち・一生の帳簿を組にして (at, P] を再生する通しの形。`spike_book_of_rows_cut_goes_on_as_the_fold_of_every_event` は待ち・一生の帳簿を at までの全件の fold で与え、`spike_wait_books_of_rows_cut_are_those_of_the_rows_at_the_position` は帳簿だけを比べるので、2 つは別々に確かめた。通しの形は 3977 の受け入れの test で確かめる。
- task 3978 は 3887 に依るので、3887 の受け入れを上の形（再生後に揃う）にするなら合わせて見直す。

## 使った時間と試作の数

- 作業: 約 40 分（09:31〜10:10 JST）と review の差し戻しへの直し約 20 分、計約 1 時間（上限 3 時間）。
- 試作: 切り方 2 通り（`Bounded`・`Range`）を全種類に当て、`Since` の下限の切り方 1 通り。種類ごとに 2 通り以内。
