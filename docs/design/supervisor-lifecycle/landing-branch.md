---
id: design-supervisor-lifecycle-landing-branch
type: design
title: "Landing branch"
status: current
created: 2026-09-27
updated: 2026-10-03
last_verified: 2026-10-03
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-up-down
  - design-supervisor-lifecycle-doctor
  - adr-t615-1
  - adr-0008
  - adr-0047
  - adr-0054
---

# Landing branch

着地先のbranch、着地後のpushのremote、pushするかは、repositoryの`dagq.toml`の`[repository]`で指定でき、指定が無ければruntimeが決める（[ADR-t615-1](../../adr/2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)。ADR-0008決定3・4・6・7・8、ADR-0047決定26、ADR-0054決定7をamends）。

**実装状況**: すべて実装済み（branchはtask 619、remoteとpushはtask 620）。`src/domain/landing_branch.rs`の`resolve`が下の順で決め、`GitRepository::landing_branch`（`src/infrastructure/adapters.rs`）がmain checkoutの`dagq.toml`の`[repository]`（`run_env.rs`の`load_repository_config`。`GitRepository::repository_config`が`branch`と`remote`をGitの名前として検査する）・pushのremoteのHEAD・ローカルのbranchを読んで呼ぶ。`main_head`・`main_history`・`main_checkout`は使うたびに解決したbranchを読み、`advance_main`（`Repository`）と`push_main`（`MainRemote`）は解決せず、`integrate`の`land_integrating`が着地ごとに1度解決した`LandingBranch`を受け取る（task 667。下の「着地ごとに1度解決する」）。`GitRepository::inspect`はbranchを読まない（`rebind`・`review`・`plan`は着地先が無くても動く）。pushは`integrate`の`decide_push`（`src/application/integrate.rs`。`push_main`がその結果をlogに出して記録する）が`MainRemote::push_config`で`[repository]`を読んで下の表のとおりに決め、既定のremoteは`landing_branch::DEFAULT_REMOTE`（`origin`）。`up`のpreflightと`doctor`は`GitRepository::repository_settings`（branchの解決と`PushTarget`。明示した`remote`が無くpushするならerror）を出す。

## `[repository]`の欄

読むのは`[run.env]`と同じmain checkoutの作業ファイルの`dagq.toml`（[Run environment](run-environment.md)）。表もkeyも省略でき、表が無ければすべて既定になる。

| key | 型 | 既定 | 意味 |
| --- | --- | --- | --- |
| `branch` | 文字列 | 無し（下の推定） | 着地先のbranchの名前（`refs/heads/`を付けない。例`"master"`） |
| `remote` | 文字列 | `"origin"` | 着地後にpushするremoteの名前 |
| `push` | 真偽値 | `true` | `false`なら着地後にpushしない |

```toml
[repository]
branch = "master"
remote = "upstream"
push = false
```

- 書式の誤りはerrorにする: 表の中の未知のkey、型の違い、空の文字列、`refs/`で始まる`branch`、`git check-ref-format --branch`に通らない`branch`、remoteの名前として使えない`remote`。
- `dagq.toml`の他の表と同じく、`[repository]`を知らない旧バイナリは未知の表として拒むので、表を足すのはそれを知るバイナリに入れ替えた後にする。dagq自身のrepositoryは表を足さない（既定の推定で今までどおり`main`と`origin`になる）。

## branchの解決

`branch`があればそれを使う。そのbranchがローカルに無い（`refs/heads/<branch>`が無い）ときは推定に落とさず、解決できないとする。

`branch`が無ければ、次の順でローカルに`refs/heads/<name>`が在る最初のものを使う。

1. pushのremote（`remote`、既定`origin`）のHEAD: `git symbolic-ref refs/remotes/<remote>/HEAD`が指す`refs/remotes/<remote>/<name>`の`<name>`（cloneが作る。無ければ`git remote set-head <remote> --auto`で作れる）。remoteが無いか、refが無いときは飛ばす。runtimeはfetchもnetworkへの問い合わせもしない。
2. `main`
3. `master`

どれも無ければ解決できない。main checkoutが今checkoutしているbranch（`HEAD`）は見ない。

解決は使うたびにその時点のrepositoryと`dagq.toml`で行い、DBにもsupervisorの登録にも保存しない。途中で指定を変えたときは、次のclaimと次の着地から新しいbranchを使う（claim済みのrunは着地のrebaseで新しいbranchに載る）。

### 着地ごとに1度解決する

1回の着地（`integrate`と、supervisorが行う着地。どちらも`land_integrating`）はbranchを開始時に1度だけ解決し、依頼文に書くrebase先の名前、`advance_main`のff先（`branch refs/heads/<branch>`をcheckoutしているworktreeの判定も含む）、pushの`refs/heads/<branch>:refs/heads/<branch>`と`push_*`のeventの`branch`に同じものを使う。着地するcommitが`[repository] branch`を書き換えると、main checkoutでの`merge --ff-only`がその`dagq.toml`を書き換えるが、その着地のpushは開始時のbranchへ行き、新しい指定は次の着地から効く。rebase先とff元のcommit（main head）は、それより前に着地の枠を取る`begin`が`main_head`で読む（その間に指定が変わると、`update-ref`は古い値の検査で、`merge --ff-only`はfast-forwardにならずに失敗し、branchは動かない）。pushのremoteと`push`（下の「pushの解決」）は今までどおり着地の後に`push_config`で読む（`--no-push`（`integrate --no-push`と、それで承認されたrunをsupervisorが着地させるとき）は`MainRemote`を渡さないので、記録する`remote`だけを`Repository`の`repository_config`で着地の後に読む）。

解決の結果は`branch`（名前）と`branch_source`（`config` / `remote_head` / `main` / `master`）で表す。

## 解決したbranchを使う箇所

今`refs/heads/main`を読んでいる箇所は、すべて`refs/heads/<branch>`を読む。

- claimのbase commit（[supervise](supervise.md)、ADR-0054決定7）と、着地開始時のmain head・rebase先・`commit-tree`の親（[integrate](integrate.md)の手順4・6、ADR-0008決定3）。
- 着地のbranchの進め方（ADR-0008決定4）: `branch refs/heads/<branch>`をcheckoutしているworktreeがあればそこで`merge --ff-only`、なければ`update-ref refs/heads/<branch> <commit> <old>`。
- main checkoutの判定のうち「着地先をcheckoutしているworktree」を探すもの（`main_checkout`）。`dagq.toml`を読むmain checkout（main worktree。[Run environment](run-environment.md#main-checkoutの決め方)）とは別。
- mainの履歴（`main_history`。`stats`の`conflict_hotspots`、claimのhotspot、`plan`）、merge-treeの事前判定、landing recheck、resumeの依頼文に書くrebase先と着地したtaskの一覧。
- runtimeがsessionやaskに書く文面は、「main」の代わりに解決したbranchの名前を書く。

## pushの解決

| 状況 | 結果 | event |
| --- | --- | --- |
| `integrate --no-push` | pushしない。remoteの有無も見ない | `push_skipped`（`reason: "--no-push"`、`remote`は着地の後に読んだ`[repository] remote`（既定`origin`。読めなければ`origin`で、着地は失敗にしない）） |
| `push = false` | remoteを見ずにpushしない | `push_skipped`（`reason: "push = false in dagq.toml"`） |
| `remote`を書かず、`origin`が無い | pushしない（今までどおり） | `push_skipped`（`reason: "the repository has no remote origin"`） |
| `remote`を書き、そのremoteが無い | 設定の誤り。着地は取り消さない | `push_failed`（`error`にremoteが無いこと） |
| 着地の後に`[repository]`が読めない | pushしない。着地は取り消さない | `push_failed`（`error`に読めない理由、`remote`は`origin`） |
| remoteが在る | `git push <remote> refs/heads/<branch>:refs/heads/<branch>` | 成功は`push_finished`。失敗してもremoteの先端に着地commitが含まれれば`push_finished`、含まれないか確認できなければ`push_failed` |

- payloadは今の`remote`・`commit`（・`reason` / `error`、`push_failed`は`code: push_failed`も）に`branch`を足す。`push_finished`の`already_delivered`は通常の成功で`false`、失敗後にremoteへの到達を確認した成功で`true`。`push_failed`のattention（`push main`）と、runが`integrated`のまま残る扱いは変えない。人の手のpushは`git push <remote> <branch>`になる。
- remote側のbranchの名前はローカルと同じで、別の名前へpushする設定は持たない。

解決の結果は`remote`・`remote_source`（`config` / `default`）・`remote_exists`・`push`で表す。

## `up`のpreflightと`doctor`

- **`up`**: cmux・Claude・trust・`[run.env]`のプログラムの検査と同じpreflightで、supervisorを起動する前に解決する。次のどれかならsupervisorを起動せず、何が解決できなかったかと、`dagq.toml`の`[repository]`に`branch`（と`remote`）を書く案内を付けたerrorで止まる: `dagq.toml`が読めない・`[repository]`の書式が誤っている、`branch`が解決できない、書いた`branch`がローカルに無い、書いた`remote`が無い（`push = false`なら`remote`は見ない）。通れば出力に`repository`（`branch`・`branch_source`・`remote`・`remote_source`・`remote_exists`・`push`）が付く。
- **`doctor`**: 状態を変えずに同じ解決を行い、`repository`の欄に上の欄と、解決できなければ`error`（`up`のerrorと同じ文面）を出す。既定の出力にも出す。
- **ほかのコマンド**: `supervise`（起動時）・`integrate`など着地先を読むコマンドは、解決できなければ同じ文面のerrorで止まる（既定のbranchを仮定しない）。`stats`は`conflict_hotspots`の`history`を`unavailable`（`reason`に同じ文面）にして残りを出す。`plan`・`rebind`・`review`は着地先を読まない。supervisorが走っている間に解決できなくなったときは、pass の先頭の検査（`check_landing_branch`）で変化を1度warnし、解決するまでclaimと着地（review が pass した run の着地と、`approve_landing`の`land`の答えで着地の列に並んだrunの着地の開始（`start_approved_landings`））を始めない（`[run.env]`のプログラムが見つからないときと同じ扱い）。`land`の答えそのものは解決しない間もその場で適用してaskを閉じ、runを列に並べる（task 949、[Review](review.md#review-supervisor)の6）。

### supervisorが確かめる頻度と条件

pass の先頭の`check_landing_branch`は、毎passで`git`を起動しない（task 1078）。解決の入力のファイルの印（`GitRepository::landing_branch_stamp`。`Repository::landing_branch_stamp`）を`git`なしで読み、前の解決が解決できていて、前に解決したときの印と同じで、前の解決から`LANDING_BRANCH_RECHECK`（5秒）が経っていなければ、前の解決の結果をそのまま使う。印が変わったpass、5秒が経ったpass、前の解決が解決できなかった間の全てのpassでは、今までどおり`landing_branch`で解決し直す（`git symbolic-ref`と`git show-ref --verify`）。解決できない間は毎passで解決し直すので、一時の失敗（`git`が起動できなかったなど）でも、入力を見落としても、解決できるようになった次のpassで再開する。印は解決の前に読むので、解決の途中で入力が変わると、次のpassの印が前の印と違い、もう一度解決する。

印に入れるファイル（それぞれの有無と、更新時刻・大きさ・inode・状態変更時刻（ctime））:

- main checkoutの`dagq.toml`（`[repository]`の`branch`・`remote`）
- Gitのcommon dirの`config`・`packed-refs`・`reftable/tables.list`（reftableのrepositoryではrefが全部ここに入る）
- pushのremoteのHEAD（`refs/remotes/<remote>/HEAD`。remoteは`dagq.toml`を今読んだもの。読めなければ`origin`）
- 解決が確かめうるbranchのloose ref（`refs/heads/<name>`）: `branch`を書いていればそのbranch、書いていなければremoteのHEADが今指すbranchと`main`・`master`

どのファイルを見るか（remoteとbranch）も毎pass今の`dagq.toml`とremoteのHEADから決め直すので、指定やremoteのHEADが変わると見るファイルも変わる。Gitはrefの作成・削除・更新をlock fileのrenameで行うのでinodeが変わり、`pack-refs`は`packed-refs`を書き換える。branchの削除・そのbranchへのcommit・remoteのHEADの付け替え・`dagq.toml`の書き換えは、次のpassの印を変える（`src/infrastructure/adapters.rs`の`the_landing_branch_stamp_follows_what_the_resolution_reads`と、`tests/it/landing_branch.rs`の`a_running_supervisor_follows_each_change_of_the_landing_branch`が、見直しの間隔を1時間にしたsupervisorで、この変化のそれぞれで次のpassに止まり・再開することを確かめる）。

5秒の上限を残すのは、印が見落としうる変化があるため: 大きさもinodeも変えないその場の書き換え（editorでない`dd`など）が、更新時刻と状態変更時刻の粒度（HFS+などでは1秒）の中で2回起きた場合。上限があるので、ADR-t615-1の「解決しない間はclaimと着地を止める」は、見落としがあっても5秒以内に当たる（再開の側は上のとおり次のpass）。そのほかに印が見ないもの（`refs/heads/<name>`自身がsymbolic refのときの指す先、`config.worktree`とincludeされたconfig）も、この上限で拾う。productionのsupervisorでも、1 passの`git`の起動が2本減る（5秒に1回は解決し直す）。

main checkoutの無いrepository（印を読めない）と、`Repository`の既定の実装（testのfake）は、今までどおり毎passで解決する。変えないもの: 着地ごとに1度の解決（task 667）、`fill_slots`のclaimで`main_head`が失敗したときの解決し直し（task 1018。印に依らずその場で解決する）、`up`のpreflight、`doctor`、supervisorの起動時の解決。

`fill_slots`のclaimのloopで`main_head`が失敗し、その場の`resolve_landing_branch`でも解決できずclaimを保留してloopを抜けるときは、tracingのINFO eventに`event = "claim_landing_branch_unresolved"`を記録する（task 1137）。これはpassの途中の再確認で保留した印で、先頭の`check_landing_branch`では出さない。先頭のstampのキャッシュで解決を省いたかどうかには依らない。`tests/it/runtime_claim.rs`の`main_vanishing_after_the_landing_branch_check_holds_the_claim`はsupervisorのJSONLの`fields.event`でこの印を確かめ、mainが先頭で消えただけでは通らない。

## 既存のqueueの互換

`[repository]`の無い`dagq.toml`（とファイルの無いrepository）では、`origin`のHEADが`main`を指すか、`main`が在れば着地先は`main`、pushは`origin`へ行う。dagq自身のrepositoryはこれに当たり、設定を足さずに今までと同じ振る舞いになる。DBのschemaとeventのkindは変わらない（payloadに`branch`が増えるだけ）。

## e2e

`tests/e2e/other_repository.rs`の`a_task_lands_on_master_of_a_repository_without_origin_cargo_toml_or_agents_md`（goal 52）が、default branchが`master`で`origin`が無く、`Cargo.toml`も`AGENTS.md`も無い使い捨てのrepositoryで、stubのproviderのtaskをclaimから着地まで通す。`doctor`の`repository`が`branch: master`・`branch_source: master`・`remote_exists: false`で、着地は`master`への1つのsquash commit（`main`は作らない）、`push_skipped`の`reason`が`the repository has no remote origin`であることを見る。
