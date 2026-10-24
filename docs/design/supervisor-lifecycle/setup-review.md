---
id: design-supervisor-lifecycle-setup-review
type: design
title: "reviewの設定の生成（setup-review）"
status: current
created: 2026-10-14
scope: runtime
related:
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-review-programs
  - design-supervisor-lifecycle-roles
  - design-authorization
  - adr-t2259-1
---

# reviewの設定の生成（setup-review）

[ADR-t2259-1](../../adr/2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)決定7の実装。
人かinboxが打つ`dagq setup-review`は、coding agentにpjを一度だけ読ませてreviewの設定を作らせ、差分を見せ、人が承認したときだけmain checkoutの作業ツリーに書く。
以降のrunのreviewは、人がcommitしたその設定だけをlanding branchから読む（[Review](review.md#reviewのsubagent)）。
`dagq init`は推奨の設定（built-inの既定のon/offと、toolchainのprofileの推奨）とこのコマンドを案内するだけで、agentを起動せず`dagq.toml`も書かない。
`dagq doctor`は、pjの規則のfileが無い有効なbuilt-inを出すときにこのコマンドを案内する。

## 流れ

1. main checkoutのHEADのcommitから、queueのdirectoryの下に一時の場所（そのcommitのworktreeと、jobの出力のdirectory）を作る。
   main checkoutの作業ツリーには触れない。
   worktreeはGitにcheckoutさせず、commitのobjectからfileを書くので、repositoryのhookとclean・smudge・processのfilterは承認の前にも動かない（`infrastructure::setup_review::materialize`）。
2. 生成のjobを一時のworktreeで起動する。
   promptはbuilt-inとその既定、書いてよいもの、HEADがcommitしている今の設定（`dagq.toml`と`.dagq/review/`の下）を持つ。
   今の設定があれば、それを入力に更新の案を作る。
   HEADの`dagq.toml`が宣言したtoolchainのprofile（無ければHEADのfileから推測したもの）が推奨するreviewのプログラムのうち、書いてよい範囲に入るものも入力に足す。
   profileのほかの推奨（宣言の案・`--verify`・containerの設定）はjobに渡さず、出力の`toolchains`に案内として出す（[Toolchain profiles](../toolchain-profiles.md#推奨の設定)）。
3. jobが終わったら、jobが変えたfileを、場所を作ったときと後の木のbytesの違いとして1度だけ読み（repositoryが無視するfileも含む）、そのbytesを適用の前に下の「境界と不変条件」で検査する。
4. 同じbytesの差分を見せ、人の承認（端末での確認か、事前の承認の`--yes`）があったときだけ、main checkoutを検査し直してから同じbytesを作業ツリーに書く。
5. 成否に依らず一時の場所を消す。
   SIGINT・SIGTERM・SIGHUPでの中断（jobの間と承認の問いの間のCtrl-Cを含む）も、jobを止め、何も書かずに消してから終わる。

## 境界と不変条件

- jobが変えてよいのは一時の場所の`dagq.toml`と`.dagq/review/`の下だけで、`dagq.toml`のうちbuilt-inのagentのtableとprogramのtableだけである。
  外のfileか外のtableの変更が1つでもあれば、何も適用せず誤りにする（commentと空行の違いは変更としない）。
- 生成した`dagq.toml`をruntimeの設定の読み出しで読めなければ、適用の前に誤りにする。
- `.dagq/review/programs/`のscriptは、program reviewがlanding branchから写して別の場所から流すので、cwdのGitのwork treeをrootにしていなければ誤りにする（[プログラムのreview](review-programs.md)）。
  commentや文字列の中の語ではなく、`git rev-parse --show-toplevel`の出力を変数に入れるか`cd`するコマンドがあることを見る。
  そのGitのコマンドは語ごとに比べ、出すものを変える別のoptionや引数を足したものは数えない。
  代入の値か`cd`の引数の全体がその出力でなければ（`/..`を連ねるなど）数えない。
  引用は行をまたいでも引用のまま読み、here-documentの本文も文字列としてコマンドに数えない（`domain::setup_review::takes_root_from_cwd`）。
- 変えたpathが通常のfileでなければ（symbolic linkなど）誤りにし、場所の外のfileを写さない。
  木はdirectoryを`O_NOFOLLOW`で開いた記述子から辿って読むので、jobが残したプロセスがdirectoryやfileをlinkに差し替えても場所の外を読まない（`infrastructure::setup_review::snapshot`）。
- 検査し、差分で見せ、適用で書くのは、jobの後に1度だけ読んで持ったbytesで、一時の場所を読み直さない。
  jobが残したプロセスが後で場所を書き換えても、検査していない中身はcheckoutに届かない。
- 変更の一覧と見せる差分は、file systemから読んだそのbytesとcommitのblobから作り、repositoryの設定や属性（`.gitattributes`を含む）のclean・smudge・processのfilter、外部のdiff、textconvを通さない。
- 一時の場所のGitはobjectを読むときだけ、場所を作ったときに読んだworktreeのGitのdirectoryで動かし、jobが書き換えうる木の中の`.git`を辿らない。
- 生成の間にmain checkoutのHEADが動いていれば、人のcommitを古い中身で戻さないよう何も適用せず誤りにする。
- 変えるfileのどれかにmain checkoutの作業ツリーでcommitしていない変更（HEADに無いpathに既にあるfileを、untrackedか無視するかに依らず含む）があれば、人の書きかけを上書きしないよう何も適用せず誤りにする。
  検査するのは書くpathそのものだけで、無視するdirectoryに別のfileがあることは妨げにしない。
  変更は`git status`で見ず、作業ツリーのbytesと実行bitをHEADのblobとfilterを通さずに比べ、indexの項目をHEADのものと比べて見る。
  indexのassume-unchanged・skip-worktreeが隠す変更も見つけ、repositoryのfilterを動かさない（filterや改行の変換でbytesが違うfileも変更とする）。
- HEADと作業ツリーの検査は差分を見せる前と承認の後の両方で行い、人が差分を読む間の変更も上書きしない。
- 書く先かcheckoutの下でそこへ至るdirectoryがsymbolic linkなら、checkoutの外や別のfileに書かないよう、どのfileも書く前に誤りにする。
- 承認が無ければ（端末が無く`--yes`も無いときを含む）何も書かない。
- 見せる差分の制御文字（ESC・CSI・`\r`など）と文字の向きを変える印は、jobが書いた中身が端末の行を隠したり書き直したりしないよう、見える形のescapeにする（`domain::setup_review::visible`）。
- 写したfileの権限は、jobが実行可能にしたかどうかだけを取って0755か0644にする。
- commit・push・queueの状態の変更はしない。
  人が差分を確かめてcommitし、着地はいつもどおり`integrate`だけが行う。
- jobの道具は一時のworktreeだけに書ける形にする（Claudeは`Edit`・`Write`をそのdirectoryに限り、Codexは人の設定の書き込み先とnetworkを足さず`/tmp`と`$TMPDIR`も書けないsandbox）。
  一時のworktreeはrepositoryのfileなので、Claudeはsettingの読み込み元を持たず（木の`.claude/`と人のsettingsのhookと許可を読まない）、Codexは木を`untrusted`にして木の`.codex/`を読まない。
  それでもhostでは助言で、適用の可否はruntimeの検査が決める。

## 権限

- コマンドを打てるのは人とinboxだけで、planner・worker・jobのroleは`authorization_denied`で拒む（[Authorization](../authorization.md#policy)）。
- 事前の承認の`--yes`は人（`DAGQ_ROLE`の無い呼び出し）だけのもので、inboxを含む人でないactorには拒む。
  inboxは`--yes`なしで打って差分を人に見せ、書くのは人が自分の端末で承認したときだけである。
- 生成のjobのrole `setup-review-job`はheadlessのjobで、queueは読むだけで、状態を変えるuse caseを持たない。
  権限の意図はfileの読みと編集だけで、queueのCLIを持たない（Claudeはshellも持たず、Codexは作業場所だけに書けるsandboxで動く。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。
  providerは`[roles.setup_review]`の`provider`、無ければClaude。
  `job_route`を通らないので、そのproviderが使えなくてももう一方へ切り替えず控えも待ちもせず、起動の失敗や0でない終了はそのままコマンドの誤りで、何も書かない。

## 入口の地図

| 知りたいこと | コードの入口 |
| --- | --- |
| 流れと検査の順 | `application::setup_review::setup_review`、hostは`SetupReviewHost`（実装は`infrastructure::setup_review::LocalSetupReview`） |
| 検査の判断 | `domain::setup_review` |
| prompt | `application::prompt::setup_review_prompt` |
| `init`の案内 | `application::setup_review::init_recommended`（repositoryは`InitRepository`、推奨の案は`domain::toolchain::recommend`） |
| `doctor`の案内 | `application::review::check_agents` |
