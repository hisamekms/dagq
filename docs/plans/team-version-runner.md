---
id: plan-team-version-runner
type: plan
title: チームの版のrunnerの具体案（pullの呼び出しとleaseの時間・jobの帰属・imageのregistryとdigest・GitHub Appの権限とruleset・hostに依る機能の移し方・資格情報の設定と記録と予算の形）
status: active
created: 2026-11-03
owners:
  - hisamekms
tags:
  - architecture
  - runtime
  - security
related:
  - adr-t3987-1
  - adr-t3985-1
  - adr-t3981-1
  - plan-team-version-identity
---

# チームの版のrunnerの具体案

[ADR-t3987-1](../adr/2026-11-03-t3987-1-team-version-runners-pull-under-fenced-run-leases-ai-jobs-on-runners-pinned-images-two-github-apps-and-attributed-claude-credentials.md)の決定を実装するときの具体の候補を置く設計資料。
ADRは方針と境界だけを持ち、ここの名前・値・形は候補で、実装のtaskが選んで定義のそばのdoc commentへ移す。
HTTPのAPIのpathと版の形はHTTPのAPIのADRが決めるので、ここの呼び出しの名前は仮の名前である。
まだ実装されていないので、`docs/design/`には書かない。
識別とtokenの候補は[チームの版の識別の具体案](team-version-identity.md)が持つ。

## pullの呼び出しとleaseの時間（決定1・2）

| 呼び出し（仮） | 中身の候補 |
| --- | --- |
| `poll` | runnerのtokenと申告（OS・arch・container・資格情報の種類）と負荷の要約を送る。仕事が無ければ長いpoll（最大25秒）で待って空を返す |
| 割り当ての応答 | run idかjob id・leaseの世代・leaseの期限・固定したimageのdigest・runner用のAppのinstallation token・資格情報の種類・run tokenの受け取り先 |
| `heartbeat` | run id・世代・負荷の要約。応答で期限を延ばす。今の世代でなければ`409 lease_lost` |
| `report` | turnの終わりのrun branchのhead・状態のファイルの中身・receipt。世代を添える |
| `release` | 止まるときに自分からleaseを返す |

- leaseの期限の候補: 90秒。生存の報告の間隔: 20秒。期限は制御側の時計で、応答に残りの秒を返す（runnerの時計の時刻を渡さない）。
- 世代: runごとの単調に増える整数。割り当てのたびに書き込みの係のtransactionで1つ上げる。
- 拒まれたrunnerの後片付け: containerをSIGTERM→10秒→SIGKILL、cloneとrun dirを消す。
- 割り当ての選び方の候補: 今のclaimの順（優先度・依存・控え）に、runnerの共有の印・申告の一致・資格情報の線引き（決定8）・予算（決定10）・負荷の閾値を足して絞る。

## jobの帰属の表（決定3）

| job | 帰属 |
| --- | --- |
| worker・resume・runのreviewのagent・triage・runの復旧 | runのtaskのgoalのowner |
| plan review | proposalのgoalのowner（新しいgoalは依頼した人） |
| goal review | goalのowner |
| planner | 計画の依頼をした人 |
| 毎時・日次の見直し・observer | チーム（チームのAPI keyだけ） |
| 着地の検証 | runのowner（検証役のマシンの選び方は着地の照合のADR） |

## imageのregistryとdigest（決定4）

- base: `ghcr.io/hisamekms/dagq-runner-base:<dagqの版>`をmulti-archのmanifestで出し、release notesにdigestを書く。
- projectのimage: tagは今の`domain::container_image`の`Images::of`の計算（baseの識別子・Containerfileの中身・入力のファイル）のまま、pjのregistryに`<registry>/<repo>/dagq-worker:<tag>`で置く。
- CIのworkflowの候補: mainへのpushでContainerfileか入力が変わったときにbuildとpushを行い、GitHub ActionsのOIDCのJWTで制御側の`image publish`（仮）に`(arch, tag, digest)`を届ける。
  制御側はJWTの`repository`・`job_workflow_ref`・`ref`（`refs/heads/main`）を配置の設定と照らし、baseのlabelのdagqの版をAPIの版と照らす。
- 制御側の記録の候補: `images(tag, arch, digest, published_by, published_at)`と、runの`pinned_image_digest`・検証の`verification_image_digest`。
- runnerは`podman pull <registry>/<repo>/dagq-worker@sha256:...`だけを使い、pullした物の`Digest`を報告に添える。

## 共有のrunnerの確かめ方（決定5）

- runnerの設定の候補: `[runner] shared = true`のとき、hostの実行の経路をbuildの段で外した実行の型だけを持つ。
- 報告に添える物の候補: containerのid・imageのdigest・podmanの`inspect`の要約（userとmount）。
- 食い違いの記録の候補: `runner_shared_revoked`（理由・報告の中身）と、管理者への運用のask。

## GitHub Appの権限とruleset（決定6）

| App | 権限の候補 | 鍵 |
| --- | --- | --- |
| runner用 | Contents: write、Metadata: read。installation tokenは`repositories`で1つに、`permissions`で`contents:write`に絞って発行（期限はGitHubの既定の1時間）。Workflows権限は与えない。leaseの間は期限の前に生存の報告の応答で新しいtokenを渡し、leaseを失えば`DELETE /installation/token`で失効させる | 制御側のsecretだけ |
| main用 | Contents: write、Workflows: write、Metadata: read。使うのは`PATCH /repos/{owner}/{repo}/git/refs/heads/main`の`force: false`と、引き継ぎでの名前空間のrefの`POST`（作成）と`DELETE`だけ | 制御側のsecretだけ |

GitHubのrulesetは「規則に当たる操作を禁じ、bypassの一覧だけが越えられる」形で、bypassはruleset全体に効く。
そのため、迂回させてよい規則と誰にも迂回させない規則を別のrulesetに分ける。
名前は`R`と番号の仮の名前で、実際のGitHubで表せるかは試行用のrepositoryで人が確かめてから選ぶ。

| ruleset | 対象 | 規則 | bypass |
| --- | --- | --- | --- |
| R1 mainの更新 | `refs/heads/main` | `update` | main用のAppだけ |
| R2 mainの削除と強制 | `refs/heads/main` | `deletion`・`non_fast_forward` | なし |
| R3 名前空間の外のbranch | `~ALL`から`refs/heads/main`・`refs/heads/dagq/**`・`refs/heads/dagq-landing/**`を`exclude` | `creation`・`update`・`deletion` | 人の役（repositoryのrole）・CI・Dependabotなど、pjが選ぶもの。runner用のAppは置かない |
| R4 tag | tagの`~ALL` | `creation`・`update`・`deletion` | releaseを出す人の役かworkflow。runner用のAppは置かない |
| R5 名前空間の強制 | `refs/heads/dagq/**`・`refs/heads/dagq-landing/**` | `non_fast_forward` | なし |
| R6 名前空間の削除 | `refs/heads/dagq/**`・`refs/heads/dagq-landing/**` | `deletion` | main用のAppだけ |

R1の例:

```json
{
  "name": "dagq-main-update",
  "target": "branch",
  "enforcement": "active",
  "conditions": { "ref_name": { "include": ["refs/heads/main"], "exclude": [] } },
  "rules": [{ "type": "update" }],
  "bypass_actors": [{ "actor_type": "Integration", "actor_id": "<main用のAppのid>", "bypass_mode": "always" }]
}
```

R2の例（bypassを持たない）:

```json
{
  "name": "dagq-main-no-delete-no-force",
  "target": "branch",
  "enforcement": "active",
  "conditions": { "ref_name": { "include": ["refs/heads/main"], "exclude": [] } },
  "rules": [{ "type": "deletion" }, { "type": "non_fast_forward" }],
  "bypass_actors": []
}
```

- 禁止の確かめ: 制御側は起動時と、割り当てとrunner用のAppのtokenを出す前（記憶は短い期限）に、runner用のAppがrepository 1つと`metadata: read`だけに絞って発行したtokenで`GET /repos/{owner}/{repo}/rulesets`と各rulesetの中身を読み、R1〜R6の規則・対象・`enforcement: active`と、全てのactorのbypassが表のとおりであること（`bypass_actors`と、runner用のAppの`current_user_can_bypass`が`never`）を照らす（[ADR-t4332-1](../adr/2026-11-10-t4332-1-ruleset-gate-checks-rules-targets-enforcement-and-every-bypass.md)）。
  `bypass_actors`は`metadata: read`の読みに返らないので、その読み方が決まるまで関門は開かない（[GitHubのruleset](../deploy/github-rulesets.md)）。
  揃わない、読めない、rulesetを使えないplanのときは、割り当てを止めて理由を`status`に出し、runner用のAppのtokenを出さず、運用のaskで知らせる。
  事後に変化を見つけて知らせるだけの運用は採らない（ADR-t3987-1決定6）。
- run branchのrefの候補: 世代ごとに`refs/heads/dagq/<run id>/g<世代>`、着地の候補は`refs/heads/dagq-landing/<run id>/g<世代>`。
- 世代の中のpushは、そのrunnerが前に報告したheadからのfast-forwardだけ（`--force`も`--force-with-lease`も使わない）。
  新しい世代のrefは後継のrunnerでなく制御側が作る（下の引き継ぎの順）。
- 引き継ぎの順（`accepted_head`が`H`、前の世代のref`g<n>`が受け入れていない先端`U`か`H`を指す場合。pushが成功して報告の受け入れの前にrunnerが止まった場合を含む）:
  1. 制御側がmain用のAppで`POST /repos/{owner}/{repo}/git/refs`（`ref: refs/heads/dagq/<run id>/g<n+1>`・`sha: H`）を打つ。`H`は`g<n>`から届くのでobjectはGitHubに在る。既に在れば作らない。
  2. `GET /repos/{owner}/{repo}/git/ref/heads/dagq/<run id>/g<n+1>`で`object.sha == H`を確かめ、queueに`g<n+1>`を確かめたrefとして記録する（`verified_ref`（仮）、書き込みの係の1つのtransaction）。
  3. 新しい世代のleaseを割り当てる。後継は`g<n+1>`からcloneし、`U`をfetchもmergeもしない。
  4. 2の記録の後にだけ、`g<n>`をmain用のAppで`DELETE /repos/{owner}/{repo}/git/refs/heads/dagq/<run id>/g<n>`する。
  - どの段で止まっても、`H`は`g<n>`か確かめた`g<n+1>`の少なくとも1つから届く。やり直しは、記録で確かめた最も新しい世代のrefから1に戻る（1と4は何度打っても同じ結果になる）。
  - 1か2が失敗する間は割り当てず、`g<n>`も消さない。
  - runの終わりの片付けは、`H`がmainに入った（着地）か、runが取り消された後にだけ、残りの世代のrefと着地の候補を同じく消す。
- 人のbypassは置かない。緊急の手当ては配置する側がrulesetを一時的に外し、その記録はGitHubのauditに残る（外している間は上の確かめに落ち、runnerへの割り当ては止まる）。

## hostに依る機能の移し方（決定7）

- runnerの常駐: `dagq runner run`（前で動く）を配り、macOSのlaunchdのplist・Linuxのsystemdのunitの例を文書に置く。
- Podman machine: `infrastructure::podman_machine`をrunnerからも使い、Linuxでは`MachineGate`を常に使えると答える実装に分ける。
- 掃除: 今の`runs/`の片付けの関数を、runnerのdata dirの下の`runner/runs/`に当てる。
- 負荷の記録: 生存の報告に`load1`・メモリの空き・ディスクの空き・走っているcontainerの数を添え、制御側の`NodeSampleStore`にrunner idを鍵として書く。
- hostのe2e: `[e2e]`の宣言はそのまま、割り当てで「ownerの共有でないrunner」に絞る。
- hostで動かすコマンドの分離（ADR-t3987-1決定3・7。準備のコマンドも同じ）: runnerの資格（runnerのtokenと更新の資格・runner用のAppのtoken・各自の契約）の置き場はrunnerのuserの`0700`のdata dirとOSの資格の置き場（macOSのKeychain・Linuxのsecret service）だけにし、コマンドはそれを読めない側で動かす。候補は次の2つで、どちらも試行で人が確かめてから選ぶ。
  - 別のOSのuser: runnerのuserとは別の専用のuser（例: `dagq-hostrun`）で、runnerが記録したheadの中身をそのuserの持つ使い捨てのdirに写してから動かす。macOSはそのuserのlaunchdの領域（`gui/<uid>`か`user/<uid>`）でe2eの実launchdが動くかを確かめる。Linuxは`systemd-run --uid=`か`setpriv`で起動する。runnerのuserのKeychainとsecret serviceは別のuserから読めない。
  - sandbox: macOSは`sandbox-exec`のprofileでrunnerのdata dir・`~/Library/Keychains`・securitydへのmach-lookupを拒む。Linuxは`bwrap`でrunnerのdata dirとsecret serviceのsocketを見せない。
  - envは空から組み、runnerのtokenと資格の場所を入れない。
  - 確かめ方: runnerはhostで動かすコマンドを受ける前に、分離した側から各資格の置き場を開く試しを流し、どれも拒まれる（`EACCES`か`EPERM`、Keychainは`errSecAuthFailed`か見えない）ことを確かめ、結果を申告に添える。制御側は確かめの結果が無いか古いrunnerにはhostで動かすコマンドを割り当てない（ADR-t3985-1決定5のとおり申告は割り当ての材料で、持ち主の改変は防げない）。

## 資格情報の設定・記録・予算の形（決定8〜10）

配置ごとの設定の候補（`dagq.toml`ではない）:

```toml
[claude]
team_api_key = true          # 制御側のsecretの置き場にkeyがあるときだけ
prefer = "personal_first"    # personal_first | team_first

[budget.team]
monthly_usd = 2000
[budget.owner]
monthly_usd = 300
```

人の設定の候補: `dagq me credentials --offer personal|none --prefer personal_first|team_first`。
runnerの`host.toml`の候補: 今のClaudeの資格情報の種類と場所の表（`infrastructure::claude_credential`が読む物）をそのまま使い、制御側へは種類だけを申告する。

記録の候補（ADR-t1486-1の実行ごとのトークンの記録に足す欄）: `credential_kind`（`team_api_key` / `personal`）・`credential_holder`（`team`か人の`(issuer, subject)`）・`attributed_owner`・`runner_id`・`usage_source`（`control_broker` / `runner_report`）・`cost_usd_estimate`。

- 線引きの照合: `credential_kind = personal`かつ`credential_holder != attributed_owner`、または共有のrunnerでの`personal`を、記録を書く同じtransactionで検出して割り当てを止める。
- 予算の単位の候補: 月ごとのUSDの見積もり（モデルの単価の表から）。ownerごと・配置全体・任意でgoalごと。
- 上限に達したときの候補: 新しい割り当ての控えの理由`budget_exceeded`、当番への運用のask、管理者の`dagq budget raise`（仮）。
