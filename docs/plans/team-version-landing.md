---
id: plan-team-version-landing
type: plan
title: チームの版の着地の具体案（検証の依頼と照合・GitHub Appのref更新・関門の設定と承認の記録・slotの測定）
status: active
created: 2026-11-04
owners:
  - hisamekms
tags:
  - architecture
  - integration
  - security
related:
  - adr-t3988-1
  - adr-t3981-1
  - adr-t3985-1
  - adr-t3987-1
  - plan-team-version-runner
  - plan-team-version-identity
---

# チームの版の着地の具体案

[ADR-t3988-1](../adr/2026-11-04-t3988-1-team-version-verified-landing-and-human-gates.md)の設計資料。
名前・書式・数値は実装の候補であり、実装のtaskが選んで定義のそばのdoc commentへ移す。
まだ実装されていないのでdocs/designは変えない。
runnerの登録・pull・世代・Appのruleset・imageの受け取りは[runnerの具体案](team-version-runner.md)、人の識別と承認の資格は[識別の具体案](team-version-identity.md)を使う。
この資料はそのAPIの版やtokenの期限を別に定めない。

## 検証のマシンと依頼（決定1・2）

制御側が常設と登録したチームのマシンの中の適格な空きを先に選ぶ。
同順位では待っている依頼の古い順に、空きの大きいマシンへ割り当て、同点は登録IDで安定させる案にする。
既存のrunnerのpullを使い、制御側から接続しない。
先の群が全て使用中か控え中ならself-hosted、そこにも適格な空きが無ければGitHub Actionsへ回し、選んだ理由を残す。
検証失敗はrunへ返し、別の群への切り替えで隠さない。
通信断やlease切れのやり直しだけを別の試行として扱う。
GitHub ActionsもOIDCで識別したrunnerとして同じ報告を送る。
必要なarch・固定image・共有の登録を満たせない候補は選ばない。
ownerのhostでだけ行えるe2eは、このfallbackとは別の関門のまま待つ。

| 候補の依頼の欄 | 意味 |
| --- | --- |
| `attempt_id`, `run_id`, `generation`, `runner_id` | 検証の試行と割り当てた今のleaseに結びつける |
| `H` | 制御側が受け入れてreviewしたrunのhead。branchの先端ではない |
| `M` | GitHubから制御側が読んだmainの基点 |
| `image_digest` | 着地の検証開始時にmain用として登録済みのOCI digest。archも固定する |
| `policy_revision`, `gate_results`, `approvals` | mainの設定の版と、対象headに対して受け入れた関門の結果と人の承認 |

検証役はrunごとのcloneでHをMへrebaseし、paths・migrationの番号・reviewのプログラム・着地の検証を流し、検証したtreeでMを唯一の親とするSを作る。
migrationの振り直しや衝突の解消で差分が変われば、新しい候補を制御側へ返してreviewと必要な承認に戻し、元のHへのreviewで押し通さない。
承認を待つ前には比較用の候補をrefに置き、承認を得た後にmessageへ承認の記録を入れてSを作る。
messageだけの変更ではtreeを変えず、検証したtreeとの一致を報告する。
候補のrefは検証の試行と段階ごとに新設し、runner用のAppでfast-forwardだけを使い、受け入れた候補は着地か取り消しまで到達可能にする。
承認前の比較用commitと、承認のmessageを付けたSは同じ親から分かれるので、比較用refをSへ更新せず、S用の別のrefを作る。
比較用refも残し、承認対象のcommitの到達性を保つ。

| 候補の報告の欄 | 意味 |
| --- | --- |
| `attempt_id`, `generation`, `H` | 依頼との取り違えと古いleaseを拒む |
| `S`, `M`, `tree_hash`, `image_digest` | squashのhash、検証基点、検証したtree、実行imageのdigest |
| `candidate_ref`, `patch_id` | 候補への到達性と承認の照合に使う差分の指紋 |
| `checks` | 必須の検証のそれぞれの結果とlogへの参照。単なる全体のsuccessだけにしない |

## 制御側の照合とmainの更新（決定2・3）

1. 書き込みの係が試行とleaseの現行世代、H、必要なreview・e2e・receipt・scopeの前提を照合する。
2. 報告のMとimageを依頼の値と完全一致で比べる。
   GitHubのcommitの取得でSの親がMだけでtreeが報告と同じことを確認し、refの取得でcandidate_refがSを指すことを確かめる。
   比較の画面に使うrefもhashへ解決し、可変なbranch名だけを根拠にしない。
3. MからSの差分をGitHubから読み、制御側の決定的なコードでpatch-idを計算して承認対象と比べる案にする。
   gitやrepositoryのコードを制御側で動かさない。
   APIの省略・打ち切り・未対応のbinary差分で同一性を確かめられない場合は、同一とみなして持ち越さず、対象commitへの新しい承認を求める。
   workerのreceiptだけから検証の成功を受け入れない。
4. mainを読み直し、Mから動いていれば更新せず新しい検証の試行へ戻す。
   mainの設定と人の資格を再読し、関門の変更や失効があれば承認を取り直す。
   Sのmessageに必要な承認者と対象commitが入っていることも照合する。
5. 外部副作用の前に試行・S・Mと更新の意図を耐久的に記録する。
   main用のAppだけが `PATCH /repos/{owner}/{repo}/git/refs/heads/main` に `{"sha":"<S>","force":false}` を送る候補にする。
   Appの鍵は制御側だけに置き、rulesetの確認はrunnerの具体案と同じ入口を使う。
   ref更新は旧hashの条件付き更新とは異なるので、事前の読み直しだけに排他性を頼らず、Sの唯一の親をMにしたこととfast-forwardの強制が最後の競合の柵になる。
6. 応答のhashとmainを読み直し、Sへの更新を記録して着地を確定する。
   応答が失われても更新の意図の記録から回復できるようにする。

| 結果 | 候補の扱い |
| --- | --- |
| mainがMから前進してSを含まない | Sを更新に再利用せず、新Mでrebase・再検証。承認はpatch-id等を再照合 |
| 応答なし、mainがSかSを祖先に含む | 更新済みとして同じ試行を確定。APIの祖先の照合ができなければ保留し、再着地しない |
| 応答なし、mainがMのまま | 同じ試行と資格・関門を再照合して同じSの更新を再試行できる |
| S・親・tree・image・世代・必須結果の不一致や欠落 | 試行を拒否し、理由を記録。runnerのimage不一致等は登録の停止の既存の規則へ送る |
| 認証・ruleset・権限の拒否、mainの巻き戻りや削除 | 更新を保留し運用のask。強制更新や別の鍵に切り替えない |
| 検証コマンドの失敗 | 失敗した検証としてrunへ返す。mainを変えない |

更新の意図を記録するtransactionとGitHubのref更新は原子的にはできない。
制御側が交代したら未確定の意図を照合し終えるまで新しい着地を始めず、旧世代からの報告は拒む。
既に送信した要求が遅れて届いても、別の候補へ前進したmainを巻き戻せないことをfast-forwardで守る。
GitHubのobjectの照合ではrunnerが実際にコマンドを実行したことまでは証明できない。
登録の信頼と失効を前提にし、この限界を成功の報告から隠さない。

## 関門の宣言の書式の候補（決定4）

repositoryのdagq.tomlに次の独立した条件を置く案にする。
ローカルの既定ではこのチーム用の関門を有効にしない。

```toml
[landing.approval]
paths = ["migrations/**", "src/security/**"]
review_confidence = ["low"]
goal_marks = ["human-landing"]
two_person = false
```

条件はORで、何も書かなくても既存のlow・scope等によるaskは残る。
pathの構文はtaskのpathsの照合と揃え、追加・削除の両方とrenameの両端を対象にする。
goalの印はgoalの属性として別に管理し、runが自分で外せないことをresourceの認可で守る。
mainから固定した設定を読んで判定し、runが設定を緩める変更を含んでも自分の承認を外せない。
無効な条件や未知の値は設定の誤りとして着地を止める。
`two_person = true`ではownerと、ownerと異なる承認資格のある人の双方を必要にする案にする。
資格のある人はADR-t3985-1の人のprincipalでaskの宛先の認可に通る人であり、管理者の役だけで宛先を越える迂回は作らない。
役割・宛先・代理の人を認める範囲はidentityとownerの決定の定義を再利用する。
inboxが人の答えを引用したanswerも承認としては拒む。

askには `https://github.com/<owner>/<repo>/compare/<M>...<candidate>` の両端を完全なcommit hashで載せる。
reviewの要約、残るconcern、人の既存の答え、当たった条件、two-personの残りの承認を添える。
対象が変わったaskへの遅いanswerは最新の候補を承認したことにしない。

## patch-idと記録の候補（決定5）

比較はMから候補の最終treeへの差分を対象に、Gitのstable patch-idと同じ正規化をする候補とする。
commitの時刻やmessageだけの違いでは承認を失わせず、計算方式の版も承認に結びつける。
正規化では空白などの区別を失うので、patch-idの一致だけで範囲の拡張なしとは決めない。
新しいpath・権限・関門やreviewの対象範囲の変化は別に検査し、疑わしければ承認を取り直す。
rebaseの後に一致しなければ承認を無効にし、新しい比較を添えてaskを更新する。
同じrunの人のconcernへの判断は引き続きreviewに渡すが、それだけで新しい差分の承認を埋めない。

| eventの候補 | 欄の候補 |
| --- | --- |
| `landing_gate_opened` | run、attempt、設定の版、当たった条件、比較URL、reviewの要約と対象head |
| `landing_approved` | ask、対象commit、M、patch-idと計算方式の版、承認者のissuerとsubject、認証・承認の時点、必要な人数の条件 |
| `landing_approval_carried` | 元の承認、元の対象commit、新しい候補commitとM、再計算したpatch-id、再照合した資格と条件 |
| `landing_update_intended` / `landing_completed` | 試行、S、M、tree、image、承認の記録への参照、GitHubから照合した結果 |

この欄は未実装の候補であり、承認は書き換え不能の記録として持つ。
承認者は表示名やmailでなくissuerとsubjectの組で残す。
取り消しや持ち越しは元の承認を書き換えずに追記する。
messageの末尾に次のtrailerを付ける案にする（複数の承認は組を繰り返す）。

```text
Dagq-Approved-By: <issuer>#<subject>
Dagq-Approved-Commit: <human-approved-candidate-hash>
Dagq-Approval: <approval-record-id>
Dagq-Approval-Patch: <algorithm-version>:<stable-patch-id>
```

S自身のhashをSのmessageに入れる自己参照は作れないので、messageには人が見た候補のcommitを記録する。
実際に着地したSはeventでその承認と結びつける。
持ち越しの場合も元の対象commitをtrailerに残し、新候補との対応はeventで残す。
承認なしのrunに承認者を捏造せず、関門が不要だった判定を記録する。

## 直列化の測定と拡張の具体案（決定6）

測定の正は制御側で受け入れた割り当て・解放・失敗・着地の時点の記録とする。
集計は作り直せるビューで、slotの増減や停止の区間を含めて再計算できるようにする。
マシン自身の時計や、単なるcontainerの稼働率を分母・分子にしない。

| 測るもの | 候補の定義・値 |
| --- | --- |
| 使用可能なslot時間 | 群ごとの登録済みで検証に適格なオンラインのslot数を時間積分。未知の容量は別計上 |
| slotの使われる割合 | 検証に占有したslot時間 / 使用可能なslot時間。群別と全体を示す |
| 内訳 | 初回検証、main前進による再検証、その他の再試行、失敗、空き。承認待ちは占有slotを返して別計上 |
| 待ち | 検証の準備完了から割り当てまで、承認待ち、e2e待ち、ref更新待ちを別に集計し中央値とp90を示す |
| 検討の開始 | ADR-t3981-1の7割超を日次の窓で3回連続、かつ各窓で着地待ちのrunが存在することを初期案にする |

閾値を超えただけでは自動で実装も切り替えもしない。
直列のための待ちと再検証の費用が主因か、承認待ち・能力不足・設備停止が主因かを内訳で分ける。
前者なら、先行Sを次のMとして検証するmerge trainを試行用repositoryで評価する。
先行の失敗や変更で後続を全て無効にし、飛ばして同じSを更新しない。
依存がなく、変更path・共有の生成物・migration等の干渉がないrunは検証を並列化できる候補にする。
同じMで検証した独立runでも最初の着地でMが変わるので、次のrunは新Mで再検証するか、先行Sを含むtrainで検証しておく必要がある。
mainへの更新は常に1列に保ち、待ちの短縮と再検証の増減、不一致からの回復を測って採否を決める。
手作業の試行は人が行い、workerの完了条件にはしない。
