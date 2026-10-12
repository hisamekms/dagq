---
id: adr-t2259-1
type: adr
title: dagqにbuilt-inのreview（agentのacceptance・security・tests・docs、programのsecrets）を同梱し、dagq.tomlのbuiltin:<名前>でenabled・paths・providerを設定し、pjの規則は一度だけの生成コマンドが.dagq/review/に出したfileだけをlanding branchのcommitから読む（ADR-t1453-1決定1・4・10、ADR-t1895-1決定6、ADR-t1470-1決定2、ADR-t1570-1決定3、ADR-t1895-2決定2、ADR-t1728-1決定1・7・13、ADR-t1942-2決定4をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t1453-1 decision 1
  - adr-t1453-1 decision 4
  - adr-t1453-1 decision 10
  - adr-t1895-1 decision 6
  - adr-t1470-1 decision 2
  - adr-t1570-1 decision 3
  - adr-t1895-2 decision 2
  - adr-t1728-1 decision 1
  - adr-t1728-1 decision 7
  - adr-t1728-1 decision 13
  - adr-t1942-2 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - review
  - eval
  - config
related:
  - adr-t1453-1
  - adr-t1453-2
  - adr-t1895-1
  - adr-t1895-2
  - adr-t1470-1
  - adr-t1570-1
  - adr-t1728-1
  - adr-t1942-1
  - adr-t1942-2
  - adr-t1420-1
  - adr-t1091-1
  - adr-0029
  - design-supervisor-lifecycle-review
---

# ADR-t2259-1: built-inのreviewを同梱し、dagq.tomlのbuiltin:<名前>で設定し、pjの規則は生成したfileだけから読む

## Context

今のrunのreviewの判断の観点は、全体のreviewのjob（受け入れ条件と文書の照合。promptにdagqの文書の方針`REVIEW_DOCS_CHECK`が固定で入る）と、pjが`.dagq/agents/`に書いたagentと`[review.programs]`のscriptだけである。外部のpjは自分でagentを書かない限り観点を持たず、全体のreviewはworktreeのinstructions（`AGENTS.md`・`CLAUDE.md`）を`REVIEW_RULES`の1文で読みにいく。

request 94で人とinboxが決めた要点: dagqは外部のpjで使う前提にする。dagqにbuilt-inのreviewを同梱し、on/offは`dagq.toml`で行う。pjの規則はcoding agentに一度だけpjを読ませて設定のfileに生成し、以降のreviewはそのfileだけを読む（reviewのたびに`AGENTS.md`を探し直さない）。built-inの中身を変えたいpjは、そのbuilt-inをoffにして自分のagentかprogramを作る。dagq自身の開発の事情（Rust・cargo・このrepositoryのdocsの規則）はbuilt-inに固定しない。

実装はagent・program・生成コマンド・eval・`REVIEW_DOCS_CHECK`の移しの数個のtaskに分かれるので、先に決定を1本にそろえる。runのreviewのreviseの上限の設定はこのADRで扱わない（Consequences）。

## Decision

1. **同梱するbuilt-in。** agentのjob（ADR-t1895-1の今のagentのjobと同じ仕組み）は`builtin:acceptance`（受け入れ条件とreceiptの主張が差分に対応しているか）・`builtin:security`（差分が足す外への送信・危険なshellの実行・資格情報の扱いの意味の判断）・`builtin:tests`（変えた判断と境界のtestの十分さ・不安定なtest）・`builtin:docs`（変えた挙動に対するpjの文書の追随）。programのjob（ADR-t1895-2）は`builtin:secrets`（差分が足す行のtoken・鍵らしい文字列）。名前はagentとprogramで重ねない（意味の判断はsecurity、文字列の検出はsecrets）。
2. **採らない候補。** scopeのprogram（taskの`--paths`の外の変更はvalidationとintegrateが既に`scope_violation`で止める。ADR-0029決定3・4）とreceiptの形のprogram（形はruntimeのvalidationが受理の時に検査し、受け入れ条件の対応づけ（ADR-t1420-1）はsummaryの自由な文で機械の検査が当てにならないので`builtin:acceptance`が判断する）は同梱しない。
3. **`builtin:acceptance`は今の全体のreviewのjobそのもの。** 別のjobにしない。全体のreviewのjobは判定の土台で常に走るので、`enabled = false`は設定の誤りにする（必須のjobを飛ばしてpassにしない。ADR-t1453-1決定6）。全体のreviewは常に全ての差分で走り、providerは`[roles.review]`だけが決める。そのため`builtin:acceptance`のtableは`enabled`だけを持ち、`paths`・`provider`を書けば設定の誤りにする（決定4・5の例外。人の判断）。
4. **設定の書式。** agentは`[review.subagents."builtin:<名前>"]`に`enabled`（真偽）・`paths`・`provider`（`builtin:acceptance`は決定3のとおり`enabled`だけ）（claude / codex。pjのagentの`provider`と同じ意味）、programは`[review.programs."builtin:<名前>"]`に`enabled`・`paths`を持つ（`script`・`args`は持たず、書けば誤り）。`builtin:`の接頭辞はbuilt-inの予約で、pjのagent・programの名前に使えば誤り、知らないbuiltinの名前も誤り。読むのは今と同じくlanding branchのcommitの`dagq.toml`（ADR-t1453-1決定4）。
5. **既定。** `dagq.toml`に書かなければ`builtin:acceptance`・`builtin:security`・`builtin:secrets`はon、`builtin:tests`・`builtin:docs`はoff。各built-inは既定の`paths`を持ち（acceptanceは全ての差分で置き換えられない。他はbuilt-inごとに定義のそばに置く）、acceptance以外は`dagq.toml`に`paths`があれば置き換える（足し合わせない）。既定の`paths`の値はADRに書かず、実装のtaskが定義のそばのdoc commentに置く（ADR-t1942-1）。
6. **pjの規則の渡し方。** built-inのagent（`builtin:acceptance`の全体のreviewを含む）は、landing branchのcommitの`.dagq/review/builtin-<名前>.md`（人が直してよい文）だけをpjの規則としてpromptに足す。reviewのたびに`AGENTS.md`・`CLAUDE.md`を探さず、worktreeのrootのinstructionsを読む指示（今の`REVIEW_RULES`）も持たない。fileが無いrepositoryのbuilt-inはpjの規則なしで汎用の判断だけを行い、worktreeのinstructionsを読まない。jobの開始のeventにpjの規則が無いこと（none）を残し、`dagq doctor`がfileの無い有効なbuilt-inを出して生成のコマンド（決定7）を案内する。worktreeにだけ置いたfile・run branchで変えたfileは使わない（ADR-t1453-1決定4）。pjの機械の規則は`.dagq/review/programs/*.sh`に置き、今の`[review.programs.<名前>]`で挙げる（置き場の約束で、runtimeの仕組みは変えない）。
7. **生成のコマンド`dagq setup-review`。** 人が初回か任意の時に打ち、coding agentにpj（instructions・文書・testと文書の置き場・言語と道具）を一度だけ読ませ、`dagq.toml`（built-inの`enabled`・`paths`・`provider`とpjのprogramのtable）・`.dagq/review/builtin-<名前>.md`・`.dagq/review/programs/*.sh`を一時の作業場所に生成し、差分を見せ、人が承認したときだけmain checkoutの作業ツリーに適用する。commit・pushはしない（人がcommitし、着地は`integrate`だけ）。pjの規則が変わったら同じコマンドで更新する。coding agentに頼るのは生成の時だけ。名前は今の`dagq review ID`の位置引数とぶつからない別のsubcommandにする。`dagq init`は推奨の設定（既定のon/offとこのコマンド）を案内する。
8. **上書きしない。** pjがbuilt-inの中身を変えたいときは`enabled = false`にして`.dagq/agents/`か`.dagq/review/programs/`に自分のものを作る。同じ名前での上書き・写して編集する経路は作らない。
9. **版の記録。** built-inのagent・programのjobの開始のeventに、built-inの名前・dagqの版（build識別子）・中身のhashを残す。
10. **eval。** `builtin:security`・`builtin:tests`・`builtin:docs`のdevとholdoutのケースをdagqに同梱し、`dagq agent eval builtin:<名前>`で測れる（ADR-t1728-1の仕組み）。同梱のケースは対象のrepositoryのGitのcommit（`base_commit`）に依らない: 各ケースは同梱のbaseline（小さなfileの集合の中身）とpatchを持ち、evalは周の一時の場所に対象のrepositoryと独立のGit repositoryを作り、baselineを決まったauthor・日時でcommitしてbase、patchを当てたcommitをheadにしてjobに渡す。対象のrepositoryの履歴・ref・worktreeは読まず変えない。ケースの集合のdigest（hold-outの1回の制限のキー）はbaselineとpatchの中身から作る。built-inのevalにはpjの規則のfileとpjのprogramを当てない。productionのケースはADR-t1728-1決定11の共通の仕組みをbuilt-inの名前に広げ、本番のrunのcommitをbaseにするので対象のrepositoryのGitから読む。`builtin:acceptance`はevalの対象にしない。
11. **`REVIEW_DOCS_CHECK`の移し先。** 全体のreviewのpromptから外し、汎用の部分（変えた挙動に対する文書の古さ・欠けを`docs_drift`にする）を`builtin:docs`に移す。コードの書き写しと経緯を`docs_drift`にする部分はdagqの文書の方針（ADR-t1942-2）なのでbuilt-inに入れず、このrepositoryでは既に`.dagq/agents/design-consistency`が持つ。このrepositoryは`dagq.toml`で`builtin:docs`を有効にする。
12. **このrepositoryの設定。** `.dagq/agents/`の8つのagent（adr-rules・architecture-boundaries・config-rules・design-consistency・migration-rules・plugin-generic・receipt-evidence・test-rules）はdagqのdocsの規則を名指すpjのagentとして残し、built-inと重なる部分もpjの側に残す（receipt-evidenceは[local-checks.md](../development/local-checks.md)の根拠の規則、test-rulesは[testing.md](../development/testing.md)の規則、config-rulesのsecretの項目は[operations.md](../development/operations.md)の規則で、どれもbuilt-inの汎用の判断より狭いpjの規則）。`builtin:docs`はon（全体のreviewが今すべての差分で文書を照合しているので`paths`は全ての差分）、`builtin:tests`はoff（test-rulesが持つ）、`builtin:security`・`builtin:secrets`は既定のon。built-inが`.dagq/review/`のfileだけを読む実装の着地の前に、このrepositoryは`.dagq/review/builtin-acceptance.md`と`.dagq/review/builtin-docs.md`を手で書いて着地させ、今の全体のreviewの規則の照合を途切れさせない。新しいkeyを`dagq.toml`に足すのは固定バイナリが読めるようになってからにする（ADR-t1453-1決定10の後半）。
13. **reviseの上限の設定は扱わない。** runのreviewのreviseの回ごとの上限（`MAX_REVISE_ATTEMPTS`、ADR-t2256-1）を`dagq.toml`で変えられるようにすることは、別のtaskが自分のADRで決める。

## Amendsの判断

どのADRも番号付きの決定を複数持ち、変えるのはその一部なので、丸ごと置き換えずamendsにする（ADR-t1091-1）。

- **ADR-t1453-1決定1**（`[review.subagents.<agent>]`の設定）: `builtin:<名前>`のtableが`enabled`・`paths`・`provider`を持ち、`builtin:`の接頭辞を予約する形を足す（決定4）。今のpjのagentのtableの形（`paths`と任意の`provider`、kebab-caseの名前）と配列のtableを採らないことは保つ。
- **ADR-t1453-1決定4**（設定と定義をlanding branchのcommitから読み、定義が無ければpassにしない）: built-inの定義と`builtin:secrets`の中身はlanding branchでなくdagqのバイナリから取る例外を置き、記録はmainのcommitに加えてbuild識別子と中身のhash（決定9）にする。`dagq.toml`とpjの規則のfileは今どおりlanding branchのcommitから試行ごとに読む。知らないbuiltinの名前は決定6の経路でpassにしない。
- **ADR-t1453-1決定10**（設定の無いrepositoryのreviewは資料・prompt・起動の引数・判定を今のまま）: 前半を変える。設定の無いrepositoryでも`builtin:security`のagentのjobと`builtin:secrets`のprogramが既定で流れ、全体のreviewのpromptはworktreeのinstructionsを読む指示を持たない（決定6）。ADR-t1470-1が一部をamendsし、ADR-t1895-1のAmendsの判断は決定10を保つとしたが、このADRはその前半について変える。後半（旧バイナリが読めないtableをこのrepositoryの`dagq.toml`に足すのは固定バイナリが対応した後）は保つ。
- ADR-t1453-1でamendsの後に有効な決定: 決定1・4・10はこのADRで一部変わり、決定2はADR-t1728-1・ADR-t1728-2、決定8はADR-t1470-1・ADR-t1895-1・ADR-t1847-1、決定5はADR-t1895-1が変えたまま、決定3・6・7・9は保つ。
- **ADR-t1895-1決定6**（agentのjobごとの設定の隔離。「repositoryの規則は、どちらもreviewのpromptが名指すinstructionsから読む」）: built-inのagentのjob（全体のreviewを含む）のpjの規則はlanding branchのcommitの`.dagq/review/builtin-<名前>.md`だけから読み、instructionsを読まない形に変える。隔離（Claudeのsetting sourcesを空、Codexの`untrusted`、読み取りだけの許可とdeny、当てる範囲はrunのreviewだけ）と、`.dagq/agents/`のpjのagentの規則の読み方は保つ。決定1〜5・7は変えない。
- **ADR-t1470-1決定2**（`REVIEW_RULES`でworktreeのrootのinstructionsとそれが名指す文書を読ませる）: 規則の読み方を変える。built-inのagentのjob（全体のreviewを含む）は`REVIEW_RULES`を持たず、pjの規則をlanding branchのcommitの`.dagq/review/builtin-<名前>.md`だけから読む。決定1（`--setting-sources ""`、`autoMemoryEnabled: false`でauto memoryも読まない、`--settings`・`--allowedTools`・`--disallowedTools`の制限）の設定とmemoryの隔離は変えずに保つ。決定3・4も変えない。
- **ADR-t1570-1決定3**（`untrusted`のCodexに`REVIEW_RULES`の1文でworktreeのinstructionsを読ませて規則を渡す）: 規則の渡し方を変える。CodexでもClaudeでも、built-inのagentのjob（全体のreviewを含む）は`REVIEW_RULES`を持たず、pjの規則をlanding branchのcommitの`.dagq/review/builtin-<名前>.md`だけからpromptに足して渡す。決定1（worktreeのprojectを`untrusted`にして`.codex`の層を読まない起動）・決定2（当てるのはrunのreviewだけ）・決定4（他のprojectの層も効かない）の起動の隔離は変えず、`untrusted`の起動も保つ。
- **ADR-t1895-2決定2**（実行するprogramと設定をlanding branchのcommitから読む）: built-inのprogramの中身はdagqのバイナリから取る。その設定（`enabled`・`paths`）とpjのprogramは今どおりlanding branchのcommitから読む。決定1・3〜7は変えない。
- **ADR-t1728-1決定1・7・13**（定義とケースを`.dagq/agents/<name>/`に置く、集合のdigestをケースの一覧とpatchの中身から作る、landing branchのprogramをケースのtreeに先に流す）: built-inの定義とケースはバイナリに同梱し、同梱のケースの集合のdigestはbaselineの中身も含め、built-inのevalにはpjのprogramを当てない（決定10）。pjのagentの定義・ケース・digest・programの当て方は保ち、他の決定は変えない。
- **ADR-t1942-2決定4**（runのreviewは両方向に照合する。今はruntimeのpromptの`REVIEW_DOCS_CHECK`が持つ）: 汎用の部分（変えた挙動に対する文書の古さ・欠け）は`builtin:docs`に移し、コードの書き写しと経緯の部分はruntimeに置かずpjのagent（このrepositoryでは`.dagq/agents/design-consistency`）が持つ（決定11）。文書の差分だけで正しいとしないこと、名前の欠けだけをずれとしないことは`builtin:docs`が引き継ぎ、ADR-t2287-1決定3が決定4に足した境界の記録の漏れを指摘しないことも保つ。決定1〜3は変えない。

## Alternatives

- **`builtin:acceptance`を全体のreviewと別のjobにする**（決定3）: 受け入れ条件とreceiptの照合は今の全体のreviewが持ち、別にすると同じ判定を2度する。`enabled = false`を許すと判定の土台が無いreviewがpassしうる。そのtableに`provider`・`paths`を許す案（`provider`をその1本の行き先にし`paths`を置き換えにする案と、`paths`を受けても意味を持たせない案）は、taskの決定4・5の書き方には沿うが、`[roles.review]`と行き先が2か所になり、全ての差分で走る土台を狭められるか効かない欄を残す。決定4・5の例外として誤りにすることは人が決めた（2026-10-12）。
- **scope・receiptの形のprogramを同梱する**（決定2）: どちらもruntimeが既に止めるか、機械の検査が当てにならない。
- **fileが無ければworktreeのinstructionsを読む移行の例外**（決定6）: workerがrun branchでinstructionsを変えればreviewの規則を変えられ、goal 188の制約（pjの規則は生成した設定fileだけから、landing branchのcommitから読む）とADR-t1453-1決定4に反する。このrepositoryは決定12の手書きのfileで途切れを防ぐ。
- **reviewのたびにcoding agentにpjを読ませる**（決定7）: 規則が毎回変わりうり、時間とtokenを毎回使う。生成を一度にし、人が差分を承認してcommitする。生成コマンドがcommitする形は、着地を`integrate`だけにする境界に反する。名前を`dagq review`の下に置くと今の`dagq review ID`の位置引数とぶつかる。
- **同じ名前での上書き・写して編集する形**（決定8）: built-inの版の更新が届かなくなり、どの判断がどの中身か分からなくなる。
- **同梱のケースを対象のrepositoryのcommitに置く**（決定10）: dagqの履歴を持たない外部のrepositoryでは`base_commit`が無く流せない。独立の一時のrepositoryにbaselineをcommitすれば、どのrepositoryでも同じケースが同じ入力になり、対象のrepositoryを読まず変えない。pjの規則とprogramを当てると、同梱のtreeに意味を持たない規則で汎用の品質の成績がpjごとに変わる。acceptanceは受け入れ条件とreceiptを持つrunが要り、同梱の小さなケースで測る形に合わない。
- **`REVIEW_DOCS_CHECK`の全体を`builtin:docs`に移す**（決定11）: コードの書き写しと経緯を指摘する部分はdagqの文書の方針で、外部のpjに固定しない。
- **このrepositoryの重なるpjのagentを消してbuilt-inに任せる**（決定12）: receipt-evidence・test-rules・config-rulesのsecretの項目はこのrepositoryの文書の狭い規則を名指し、汎用のbuilt-inはそれを知らない。

## Consequences

- 実装はgoal 188の後続のtask（built-inのagent、program、`dagq setup-review`と`dagq init`の案内、eval、`REVIEW_DOCS_CHECK`の移しと`REVIEW_RULES`の削除）が行う。名前・欄・eventの綴りと既定の`paths`の今の姿は、実装と一緒に定義のそばのdoc commentと[Review](../design/supervisor-lifecycle/review.md)・[Run environment](../design/supervisor-lifecycle/run-environment.md)が持つ。実装の着地までは今の実装が動く。
- 設定の無いrepositoryでも`builtin:security`と`builtin:secrets`のjobが流れ、reviewの時間とtokenが増える。
- goal 155のsecurityのagent（task 1906）はこのrepositoryの規則（[Security](../design/security.md)の境界と権限）を名指すpjのagentで、`builtin:security`は外部のpjにも当たる汎用の意味の判断である。重なる部分はpjの側に残し、片方で他方を置き換えない。
- runのreviewのreviseの回ごとの上限を`dagq.toml`で変えることは、ADR-t2256-1の後に別のtaskが自分のADRで決める（このADRでは扱わない）。
