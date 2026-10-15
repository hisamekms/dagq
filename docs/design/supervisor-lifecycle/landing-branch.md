---
id: design-supervisor-lifecycle-landing-branch
type: design
title: "Landing branch"
status: current
created: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-up-down
  - design-supervisor-lifecycle-commands
  - adr-t615-1
  - adr-0008
  - adr-0047
  - adr-0054
---

# Landing branch

着地先のbranch、着地後のpushのremote、pushするかは、repositoryの`dagq.toml`の`[repository]`で指定でき、指定が無ければruntimeが決める（[ADR-t615-1](../../adr/2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)。ADR-0008決定3・4・6・7・8、ADR-0047決定26、ADR-0054決定7をamends）。

**実装状況**: すべて実装済み。
`src/domain/landing_branch.rs`の`resolve`が下の順で決め、Gitのadapter（`src/infrastructure/adapters.rs`）がmain checkoutの`dagq.toml`の`[repository]`・pushのremoteのHEAD・ローカルのbranchを読んで呼ぶ。
mainを読む操作は使うたびに解決したbranchを読み、mainを進める操作とpushは解決せず、`integrate`が着地ごとに1度解決した`LandingBranch`を受け取る（下の「着地ごとに1度解決する」）。
`GitRepository::inspect`はbranchを読まない（`rebind`・`review`・`plan`は着地先が無くても動く）。
pushは`integrate`の`decide_push`（`src/application/integrate.rs`）が`[repository]`を読んで下の表のとおりに決める。
`up`のpreflightと`doctor`は`GitRepository::repository_settings`（branchの解決と、pushの宛先とpushするか。明示した`remote`が無くpushするならerror）を出す。

## `[repository]`の欄

読むのは`[run.env]`と同じmain checkoutの作業ファイルの`dagq.toml`（[Run environment](run-environment.md)）。表もkeyも省略でき、表が無ければすべて既定になる。
keyと既定の意味は`domain::landing_branch::RepositoryConfig`のdoc comment。

- 書式の誤りはerrorにする: 表の中の未知のkey、型の違い、空の文字列、`refs/`で始まる`branch`、`git check-ref-format --branch`に通らない`branch`、remoteの名前として使えない`remote`。
- `dagq.toml`の他の表と同じく、`[repository]`を知らない旧バイナリは未知の表として拒むので、表を足すのはそれを知るバイナリに入れ替えた後にする。
  dagq自身のrepositoryは表を足さない（既定の推定で`main`と`origin`になる）。

## branchの解決

`branch`があればそれを使う。そのbranchがローカルに無い（`refs/heads/<branch>`が無い）ときは推定に落とさず、解決できないとする。

`branch`が無ければ、次の順でローカルに`refs/heads/<name>`が在る最初のものを使う。

1. pushのremote（`remote`）のHEAD: `git symbolic-ref refs/remotes/<remote>/HEAD`が指す`refs/remotes/<remote>/<name>`の`<name>`（cloneが作る。無ければ`git remote set-head <remote> --auto`で作れる）。
   remoteが無いか、refが無いときは飛ばす。
   runtimeはfetchもnetworkへの問い合わせもしない。
2. `main`
3. `master`

どれも無ければ解決できない。main checkoutが今checkoutしているbranch（`HEAD`）は見ない。

解決は使うたびにその時点のrepositoryと`dagq.toml`で行い、DBにもsupervisorの登録にも保存しない。途中で指定を変えたときは、次のclaimと次の着地から新しいbranchを使う（claim済みのrunは着地のrebaseで新しいbranchに載る）。

### 着地ごとに1度解決する

1回の着地（`integrate`と、supervisorが行う着地。どちらも`land_integrating`）はbranchを開始時に1度だけ解決し、依頼文に書くrebase先の名前、mainを進める操作のff先（`branch refs/heads/<branch>`をcheckoutしているworktreeの判定も含む）、pushの`refs/heads/<branch>:refs/heads/<branch>`と`push_*`のeventの`branch`に同じものを使う。
着地するcommitが`[repository] branch`を書き換えると、main checkoutでの`merge --ff-only`がその`dagq.toml`を書き換えるが、その着地のpushは開始時のbranchへ行き、新しい指定は次の着地から効く。
rebase先とff元のcommit（main head）は、それより前に着地の枠を取る`begin`がmainを読む操作で読む（その間に指定が変わると、`update-ref`は古い値の検査で、`merge --ff-only`はfast-forwardにならずに失敗し、branchは動かない）。
pushのremoteと`push`（下の「pushの解決」）は着地の後に`push_config`で読む（`--no-push`（`integrate --no-push`と、それで承認されたrunをsupervisorが着地させるとき）はpushに使うremoteの口を渡さないので、記録する`remote`だけをrepositoryが`[repository]`を読む操作で着地の後に読む）。

## 解決したbranchを使う箇所

着地先を読む箇所は、すべて`refs/heads/<branch>`を読む。

- claimのbase commit（[supervise](supervise.md)、ADR-0054決定7）と、着地開始時のmain head・rebase先・`commit-tree`の親（[integrate](integrate.md)の手順4・6、ADR-0008決定3）。
- 着地のbranchの進め方（ADR-0008決定4）: `branch refs/heads/<branch>`をcheckoutしているworktreeがあればそこで`merge --ff-only`、なければ`update-ref refs/heads/<branch> <commit> <old>`。
- main checkoutの判定のうち「着地先をcheckoutしているworktree」を探すもの（`main_checkout`）。`dagq.toml`を読むmain checkout（main worktree。[Run environment](run-environment.md#main-checkoutの決め方)）とは別。
- mainの履歴（`main_history`。`stats`の`conflict_hotspots`、claimのhotspot、`plan`）、merge-treeの事前判定、landing recheck、resumeの依頼文に書くrebase先と着地したtaskの一覧。
- runtimeがsessionやaskに書く文面は、「main」の代わりに解決したbranchの名前を書く。

## pushの解決

| 状況 | 結果 | event |
| --- | --- | --- |
| `integrate --no-push` | pushしない。remoteの有無も見ない | `push_skipped`（`remote`は着地の後に読んだ`[repository] remote`で、読めなくても着地は失敗にしない） |
| `push = false` | remoteを見ずにpushしない | `push_skipped` |
| `remote`を書かず、`origin`が無い | pushしない | `push_skipped` |
| `remote`を書き、そのremoteが無い | 設定の誤り。着地は取り消さない | `push_failed` |
| 着地の後に`[repository]`が読めない | pushしない。着地は取り消さない | `push_failed` |
| remoteが在る | `git push <remote> refs/heads/<branch>:refs/heads/<branch>` | 成功は`push_finished`。失敗してもremoteの先端に着地commitが含まれれば`push_finished`、含まれないか確認できなければ`push_failed` |

- remote側のbranchの名前はローカルと同じで、別の名前へpushする設定は持たない。

## `up`のpreflightと`doctor`

- **`up`**: cmux・Claude・trust・`[run.env]`のプログラムの検査と同じpreflightで、supervisorを起動する前に解決する。
  次のどれかならsupervisorを起動せず、何が解決できなかったかと、`dagq.toml`の`[repository]`に`branch`（と`remote`）を書く案内を付けたerrorで止まる: `dagq.toml`が読めない・`[repository]`の書式が誤っている、`branch`が解決できない、書いた`branch`がローカルに無い、書いた`remote`が無い（`push = false`なら`remote`は見ない）。
  通れば出力に解決の結果が付く。
- **`doctor`**: 状態を変えずに同じ解決を行い、解決の結果と、解決できなければ`up`と同じ文面のerrorを出す。
  既定の出力にも出す。
- **ほかのコマンド**: `supervise`（起動時）・`integrate`など着地先を読むコマンドは、解決できなければ同じ文面のerrorで止まる（既定のbranchを仮定しない）。
  `stats`はhotspotの履歴を使えないものとして同じ文面の理由を付け、残りを出す。
  `plan`・`rebind`・`review`は着地先を読まない。
  supervisorが走っている間に解決できなくなったときは、pass の先頭の検査（`check_landing_branch`）で変化を1度warnし、解決するまでclaimと着地（review が pass した run の着地と、`approve_landing`の`land`の答えで着地の列に並んだrunの着地の開始）を始めない（`[run.env]`のプログラムが見つからないときと同じ扱い）。
  `land`の答えそのものは解決しない間もその場で適用してaskを閉じ、runを列に並べる（[Review](review.md#review-supervisor)の6）。
- **保留の記録**: supervisorは解決できない保留の変わり目だけ、自分の最新の記録と違うときに`landing_branch_unresolved` / `landing_branch_resolved`を記録し、解決し直すpassごとには記録しない（`domain::claim_hold::OwnHold`）。
  `candidates`の`held`は生きたsupervisorの記録だけを読み、`status`は登録された各supervisor（生きていないものも）の項目にそのsupervisor自身の最新の保留を出す。
  どちらもattentionにはしない。

### supervisorが確かめる頻度と条件

pass の先頭の`check_landing_branch`は、毎passで`git`を起動しない。
解決の入力のファイルの印（`GitRepository::landing_branch_stamp`）を`git`なしで読み、前の解決が解決できていて、前に解決したときの印と同じで、前の解決から再確認の上限の時間が経っていなければ、前の解決の結果をそのまま使う。
印が変わったpass、上限が経ったpass、前の解決が解決できなかった間の全てのpassでは、`landing_branch`で解決し直す（`git symbolic-ref`と`git show-ref --verify`）。
解決できない間は毎passで解決し直すので、一時の失敗（`git`が起動できなかったなど）でも、入力を見落としても、解決できるようになった次のpassで再開する。
印は解決の前に読むので、解決の途中で入力が変わると、次のpassの印が前の印と違い、もう一度解決する。

どのファイルを見るか（remoteとbranch）も毎pass今の`dagq.toml`とremoteのHEADから決め直すので、指定やremoteのHEADが変わると見るファイルも変わる。
Gitはrefの作成・削除・更新をlock fileのrenameで行うのでinodeが変わり、`pack-refs`は`packed-refs`を書き換える。
branchの削除・そのbranchへのcommit・remoteのHEADの付け替え・`dagq.toml`の書き換えは、次のpassの印を変える。

上限を残すのは、印が見落としうる変化があるため: 大きさもinodeも変えないその場の書き換え（editorでない`dd`など）が、更新時刻と状態変更時刻の粒度（HFS+などでは1秒）の中で2回起きた場合。
上限があるので、ADR-t615-1の「解決しない間はclaimと着地を止める」は、見落としがあっても上限の間に当たる（再開の側は上のとおり次のpass）。
そのほかに印が見ないもの（`refs/heads/<name>`自身がsymbolic refのときの指す先、`config.worktree`とincludeされたconfig）も、この上限で拾う。

main checkoutの無いrepository（印を読めない）と、`Repository`の既定の実装（testのfake）は、毎passで解決する。
変えないもの: 着地ごとに1度の解決、`fill_slots`のclaimでmainを読む操作が失敗したときの解決し直し（印に依らずその場で解決する）、`up`のpreflight、`doctor`、supervisorの起動時の解決。

`fill_slots`のclaimのloopでmainを読む操作が失敗し、その場の解決し直しでも解決できずclaimを保留してloopを抜けるときは、tracingのINFO eventに`event = "claim_landing_branch_unresolved"`を記録する。
これはpassの途中の再確認で保留した印で、先頭の`check_landing_branch`では出さない。
先頭のstampのキャッシュで解決を省いたかどうかには依らない。

## 既存のqueueの互換

`[repository]`の無い`dagq.toml`（とファイルの無いrepository）では、`origin`のHEADが`main`を指すか、`main`が在れば着地先は`main`、pushは`origin`へ行う。
dagq自身のrepositoryはこれに当たり、設定を足さない。
