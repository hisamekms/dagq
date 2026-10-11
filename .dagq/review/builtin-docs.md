# このrepositoryの文書の置き場と照合の規則（builtin:docs）

差分と文書を照合するときに当てる、このrepositoryの規則の要点。
各項の出典のpathは参照で、規則はこの文書の文が持つ。
designへのコードの書き写しと経緯の混入、summaryの探した名前の検査は`.dagq/agents/design-consistency`が持つので、ここでは扱わない。

## 文書の置き場と役割

出典: `docs/README.md`、`docs/development/documents.md`の「AGENTS.md」「pluginの汎用性」。

- `docs/adr/`: なぜその決定をしたか（判断の記録）。append-onlyで、決定を変えるのは`amends`か置き換えの新しいADR。索引`docs/adr/INDEX.md`はfrontmatterから生成するgitignoreのfileで、直さない（ずれはADRのfrontmatterで直す）。
- `docs/design/`: 今の実装がどうなっているか（流れ・責務と境界・不変条件・コードから読めない約束・コードへの入口）。
- `docs/development/`: このrepositoryの開発の今の規則（手元の検証・test・migration・文書・taskの登録・運用）。規則の正本はここで、rootの案内・plugin・reviewのagentの定義は本文を写さずここを指す。
- `docs/plans/`: これから作るものの順序と完了条件、測定と経緯。
- `plugins/claude-dagq/`: どのrepositoryにも当たるdagqの汎用の操作（skillとreference）。このrepositoryの規則・値・経緯を持たない。
- rootの開発の案内の文書は案内だけを持ち、規則の本文を足す差分は正本（`docs/development/`）へ移す。
- 今の設定値は`dagq.toml`（値の理由はそのコメント）が持ち、文書は値を写さず指す。
- task・goalの一覧・状態・依存・完了はqueueだけが持ち、文書に写さない。

## 照合の対象

出典: `docs/development/documents.md`の「workerの文書の照合」。

- 差分と照合する文書は、taskが名指す文書と、変えた挙動を説明する`docs/design/`・`docs/development/`・pluginの`SKILL.md`と`reference/`・ADRの索引。
- 役割と権限を変える差分は、その説明を持つ文書を全て照合する（`docs/design/overview.md`の用語集・`docs/design/authorization.md`・`docs/design/supervisor-lifecycle/roles.md`など）。testやscriptが何を検査するかを列挙する節も照合する。

## ずれとするもの・しないもの

出典: `docs/development/documents.md`の「design」「workerの文書の照合」。

- design文書の対応と、名前が無いことだけをずれにしないことの検査は`.dagq/agents/design-consistency`が持つ。ここでは`docs/development/`・plugin・ADRの索引を含む全ての文書について照合する。
- 文書が差分と食い違うときは、taskが名指したかに依らず`docs_drift`にする。
- 文書の差分があることだけでは正しいとしない。`summary`が文書を直さない理由も他の主張と同じく確かめる。
- taskのpathsの外のずれは差分で直さず、`docs_drift`のfollow_up（pathと節つき）にあること。

## 文書の形

出典: `docs/development/documents.md`の「ADR」「plans」「frontmatter」「権限の表を写す文書」「AGENTS.md」。

- ADRは1つの決定（密に結びついた数個まで）で、変えるのに人の判断が要るものを書く。番号付きの決定の一部を変えるときは`amends`に変える決定を書き、元のADRの`amended_by`を足し、同じ変更で今の姿（designかdoc comment）を直す。
- 計画のステップの状態が変われば`docs/plans/current.md`を直し、完了した計画は消さず`completed`にする。
- ADR以外の文書のfrontmatterは`updated`・`last_verified`を持たず、日付の行は日付だけを書く。
- 権限の表（`docs/design/security.md`・`docs/design/authorization.md`）を変える差分は、同じ変更でpluginの`dagq-planner`の「Where your authority ends」と`dagq`の`reference/authority.md`を直す。
