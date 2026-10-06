---
id: design-supervisor-lifecycle-task-replanning
type: design
title: 長期化した非対話taskの診断・保留・成果保存・置換（未実装）
status: draft
created: 2026-10-05
updated: 2026-10-06 # task 1521 revise 1: goal review and close after replacement
last_verified: 2026-10-06 # task 1521 revise 1: ADR consistency checked; entire design remains unimplemented
scope: runtime
related:
  - adr-t1521-1
  - adr-t1521-2
  - adr-t1487-1
  - adr-t1394-1
  - adr-t451-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-goal-review
---

# 長期化した非対話taskの再計画（未実装）

**全節が予定の契約であり未実装。** [ADR-t1521-1](../../adr/2026-10-05-t1521-1-diagnose-before-replanning-and-preserve-acceptance.md)と[ADR-t1521-2](../../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)を実装する際の正本。現在のrevise・resume・requestは関連文書が持つ。この文書を追加しても#1405のrunを停止・編集しない。wrapperはtask 1440が統一するbackgroundの経路を前提にし、cmux workspaceのcloseで保留や復元を代用しない。Claude/Codexで同じ状態機械を使う。

## 入口と診断

フローは **候補検知 → 診断 → 継続か保留 → snapshot → plannerの置換proposal → plan review → trusted runtimeの適用**。手動の再計画依頼・workerの提案・runtimeの検知は同じ候補記録に合流する。人/inboxは対象runを参照した既存の永続requestを入口にできる。workerは自分のrunに提案と根拠を残すだけで、停止・実行中taskの編集・request登録・DB操作は行えない。権限上の問いだけは従来のworker_questionを使い、単なる分割の推奨をaskにしない。再計画を明示した手動requestは通常のplanner起動より先に候補へ結び、snapshot確定まで起動を待たせる。runtimeは採用した候補を一意の再計画操作に結び、同じrun・同じ診断窓の重複をまとめる。

診断は読み取りだけの復旧jobの契約を拡張し、根拠event ID・時刻の窓・task/goalの条件・paths/verify/evidence・receiptとHEAD・原因別のturn/review/resume・前回の診断と進捗を渡す。実行中turnを止めて診断する必要はない。診断結果の選択はcontinue / replan / askで、原因・比較した案・推奨・confidence・次に確かめる条件を持つ。runtimeは診断の対象versionを再確認する。

| 原因 | 診断の根拠 | 通常の次の手 |
| --- | --- | --- |
| 局所的な不具合、test/evidence/docsの欠落 | 指摘と差分が局所で、残作業が減っている | 同じ条件でrevise/resumeを継続 |
| acceptanceの曖昧さ・矛盾・実現不能 | 条件と先例・ADR・観測が食い違う | 解釈できれば記録して継続、意図変更が要ればscopeの判断 |
| 作業単位が過大、独立な条件が混在 | 条件ごとの進捗と成果の境界を分けられる | 全条件を保つ分割・範囲再配分 |
| rebase/precheckの衝突 | pass済みのHEAD、衝突event、mainの進み | 既存の衝突解消や継承retryを先に比較、総turnで分割しない |
| e2e/検証の失敗 | 対象commit・失敗log・test名・環境 | コードの修正かhost/環境の復旧。関門の削除を分割と呼ばない |
| provider/認証/資源、runtimeの障害 | turn failure、hold、復旧log | provider切替・hold・復旧。作業単位を小さくしても直るとは限らない |

原因は複数付けられ、主因と副因を記録する。高い確信度で元条件・成果を保てる推奨はADR-t451-1どおり自律で採用する。scope/discardの既存の権限ある判断で決まらない意図変更・成果破棄、low confidence、上限・復旧不能だけ人へ上げる。人の判断はask/answerのauthority・approvalと元eventを参照し、plannerの推測で代用しない。

## 永続状態と所有権

以下の名前は追加する再計画操作の状態で、既存run statusに暗黙に読み替えない。操作はoperation ID、元task/run、goal、起点（manual/worker/runtime）、診断窓、task/goal/依存のversion、単調増加するgeneration、lease token、戻り先phase、snapshot ID、request/proposal ID、ask ID、結末と理由を永続する。状態と遷移eventは同じDB transactionに書く。

| 状態 | 許される進行と保持 |
| --- | --- |
| candidate / diagnosing | 読み取り診断。元runは通常どおり進む。条件やHEADが変われば再診断 |
| continuing | 保留せず同じ条件で継続した結末。根拠を記録して操作を閉じる |
| pause_requested / quiescing | 次turn・通常receipt処理・review/e2e/landingを封鎖し、書き手の終了を待つ |
| snapshotting | 書き手が無いまま保存。元taskはin_progress、claimを拒む |
| parked / planning | 完全なsnapshotとrequestが永続済み。run slotを返し、plannerの枠で計画 |
| reviewing / awaiting_answer | 子はsubmittedでclaim不可。旧成果・元task・全後続の保護を継続 |
| applying | 永続成果と版を確認する適用の準備。DB確定前なら依存は元のまま |
| applied | 一括確定済み。元task/runはreplaced、子と対応表と依存が可視 |
| continuing_after_park / withdrawn | snapshotを保持し元条件の継続を確定。withdrawnは案の撤回で、task cancelではない |
| blocked | 停止・保存・起動・review・適用の失敗。どの段から修正するかとエラーを保持し、旧成果を残す |

保留のguardはtask/runの最新性とgenerationで判定し、lease行の有無だけでは解除しない。staleなleaseをadoptする側はoperationも同時に引き継ぎ、generationを更新する。旧tokenの書き込み・配送・適用を拒む。leaseは排他制御、versionは診断/承認した内容の一致を保証し、どちらも必要。exec引き継ぎでも同じ記録から状態を再構成する。process内の変数やrun dirだけをSSOTにしない。

## 停止境界と競合

1. trusted runtimeはtransaction内で元task/runのversion、現在phase、lease、未着地を再検査し、pause_requestedとgenerationの柵を確定する。全てのturn配送・askのanswer配送・revise・resume・provider retry・receipt受理・自動recover/triage・landing開始がこの柵を見る。既に渡したrequestも旧generationとして次turnを開始させない。
2. 原則は現在のbackground turnが終わる境界で止め、次turnを起動しない。pauseの待ちは既存のturn上限までとし、超過・緊急停止では記録したwrapper/agentと所属を再確認した子孫をpid/process groupで止める。名前でsignalしない。wrapperのack、turnの終了、書き手processの不在を確認する。孤児helperやworktreeの書き手が残ればparkedにせずblockedで復旧へ渡す。
3. validating/review/triageのjobが動いていれば、その有界な終了を待つか当該jobだけを停止して終了を確認する。verdictは履歴に残すが、古いgenerationのrevise・ask・landingを適用しない。e2e実行中も終了とfixture cleanupを待ち、結果を保存する。e2e待ち/再試行待ちなら新しい試行を止める。失敗したe2eをpass扱いにしない。
4. integratingが先に所有権を得た場合は再計画を適用しない。integrateを途中で止めてsnapshotしない。着地が終われば候補を不要として閉じ、needs_sessionへ戻れば改めて診断する。pauseが先ならbegin_integration、手動integrateを含む全着地経路を拒む。review passやintegration_approvedがあってもguardを迂回しない。
5. Git操作（rebase/cherry-pick等）が途中なら、conflict状態・index・作業treeと操作metadataを保存できるまで保存の失敗として保持する。HEADだけをcleanな成果とみなさない。復元は専用のscratch worktreeで行い、元worktreeを上書きしない。

遅延receiptは受け取ったbytesと時刻・旧generation・拒否理由を診断材料として保存する。pauseの柵より前に受理しても、その後のvalidation/review/landingが柵を再検査する。柵より後のreceiptやworker終了はcompletedへの遷移を起こさない。applied後の旧workerの終了もreplacedをfailed/completedに変えない。書き手不在の後にHEADや保存対象が変わったらsnapshotは無効で、封鎖したまま原因を調べる。

## snapshotと保存の順

snapshotは引き継ぎの材料で、receiptのsucceededやreview passの代わりではない。trusted runtimeがworkerの書けない保存先に作り、内容hashとmanifestを永続記録に結ぶ。読むfileはlink/FIFOを辿らず、通常fileを上限付きで読み、許されないfile・容量超過・I/O失敗を黙って省かずblockedにする。

保存するもの: base/HEADと元runのcommit範囲、Git refで固定したcommit、staged/unstaged差分とindex、untrackedソースと証拠、進行中Git操作metadata、task/goalの元条件・paths/verify/evidenceと全前後依存の写し、receiptの全体とhash、review/検証/e2eのlogと対象commit、turn/request/会話session ID・provider・会話の保存可能な参照、診断eventとask/answer。外部transcriptの参照だけで復元可能とは言わず、必要な引き継ぎ文面と会話の材料を保存する。target等の再生成可能なbuild出力は除外をmanifestに明記してよい。元taskのpaths外の成果も隔離保存し、子の成果として採用できるかはplan reviewで判断する。

順序は **pauseの柵 → 書き手停止確認 → 保存準備のmanifest → ref固定と一時dirへの保存 → hash/復元可能性の照合 → atomic renameでsnapshot確定 → DBでsnapshot参照とparked・永続requestを同時確定 → slot解放 → planner起動**。file/refとSQLiteを跨ぐtransactionとは呼ばない。保存準備のoperation IDを鍵に、再起動後に確定済みdir/refを照合してDB確定を再試行する。未完のdirは成果を捨てず補修する。置換起点でsnapshotが無いrequestのplannerを起動しない。DB確定前に掃除や旧branch/worktreeの削除をしない。

保存されたソース・証拠・refと元worktreeはpinする。適用直後も子の引き継ぎ確認が済むまでpinを外さず、その後は元のsnapshotを履歴の成果として保持する。自動cleanupは再生成可能なbuild出力だけを削除できる。保存成果の削除・採用しない選択は対象と理由、権限あるdiscard判断を別に記録する。容量不足も破棄の承認とみなさない。

## 元条件と置換proposal

plannerはsnapshotを読むだけで、元worktreeで修正しない。修正案のtaskをdraftからsubmitし、一つのoperationには一つの有効な置換proposalだけを結ぶ（reviseは同じproposalのversionを進める）。普通のproposalに子を紛れ込ませても元taskは終了しない。案はcontinue / split / redistribute / withdrawを区別し、破棄は独立のdispositionとして明示する。

元acceptanceを文面とordinal/hashで固定し、重複した同じ文面も別の項目として対応表に載せる。goal acceptanceも同じように参照する。以下は書式の例でありtaskの一覧ではない。

| 元条件 | 現在の根拠と残作業 | 子の条件と成果の所在 | 扱い・判断の根拠 |
| --- | --- | --- | --- |
| A1: APIの振る舞い | snapshot Sのcommit C、残る境界test | 子XのX1、Cの範囲を継承 | split、未達の検証はXで実施 |
| A2: 運用と検証 | 文書差分は保存、統合検証は未達 | 子YのY1、YはXに依存 | redistribute、同じgoalに残す |
| A3: goalの追加要件 | 根拠不足 | 対応する子が無い | 削除を求めるならscopeのauthority/answerを必須にする |

権限ある削除の判断を別に記録した項目を除き、全項目を少なくとも一つの子条件へ割り当て、複数の子で共同で満たすなら全てを載せる。既存mainに着地済みの根拠で充足する項目はcommit・検証の証拠と検査する子を載せる。元runの未着地成果だけで「達成済み」にしない。子のpaths/verify/evidence、依存DAG、成果の採用範囲と採用順・重複適用防止、残るreview指摘・e2e/衝突の対応を含める。各子のgoalは元goalと同じで、goal無しなら同じ目的を保持するgoalの設定をproposalで明示する。スコープ縮小は残りを子に残し、goal acceptanceそのものの削除はauthority・actor・対象旧新値・理由・ask/answer/event IDを要する。

plan reviewは通常の規則に加え、対応の漏れ/削除/重複、成果の所在と安全な採用、子の検証、全後続の対応、循環・claimとの競合、authorityを検査する。機械的な検査はID/version・全項目の被覆・goal一致・依存DAG・guard、意味上の充足はjobが行う。passまたは権限内のhighのready推奨は「このproposal versionを適用可能」にするだけで、この段で子をreadyにしない。reviseは案を直し、low/scope/discardや上限は既存approve_plan/planner_questionへ送る。review中に子や条件が変われば承認は無効。

## trusted runtimeの原子的適用と依存

snapshotの完全性を再確認した後、BEGIN IMMEDIATEの中で次を全部再検査する: operationが適用待ち、lease token/generation、元task/runが最新で保留中、旧書き手不在の停止証明、元task/goal・proposal・全前後依存のversion、reviewが検査したproposal hash、必要なscope/discard承認、後続が未claim、循環なし。停止証明は停止したwrapperが新しいagentを登録できないgenerationの柵と組み合わせる。fileの照合後に内容が変わらないtrusted保存領域を使う。

一つのDB transactionで元task/runを新しい終端の`replaced`にする（completedでも通常canceledでもない）。置換先IDs、条件対応・snapshot参照、全子のready化、全後続依存の付け替え、requestの適用結末、旧runの着地/claim/receipt権限の失効、operation appliedとeventを確定する。goal acceptanceの変更を権限ある判断で認めた案なら、その旧新値とauthorityもこのtransactionで更新・記録する。失敗はrollbackで子も元依存もそのまま。DB確定後の通知・表示・掃除は冪等な後処理にする。再配送にはoperation IDの既存結末を返し、二重に子を作らない。

全ての直接後続について、元taskへのedgeを、その後続に必要な子**全て**へのedgeに置換する。元taskが全部の条件を表したedgeは既定で全子を必要とする。一部だけ必要ならplannerが対応表で理由を示し、plan reviewが条件の漏れが無いと確認した場合だけ絞る。推移的な後続もこのDAGから待つので、全後続の一覧とbefore/afterをproposalと適用記録に残す。子は元taskの前提依存を引き継ぎ、子どうしの依存も検査する。replacedを依存充足と読む逃げ道を作らず、後続のclaimは必要な子全てのcompleted（着地）を要求する。子のcancel・再置換・失敗は自動でedgeを満たさず、再置換なら同じ手続きで次の全子へ展開する。

保留前に後続がclaim済みなら、この操作でそのrunを編集/停止せず、適用を拒んで再診断する。claimと依存編集は同じtransactionでguard/versionを確認するため、確認後の割り込みでも先に確定した側だけが進む。通常edit/cancel/ready/retry/recover/integrateやproposalの普通のready化は、保留中の元taskと置換候補の子・保護されたedgeへの変更をuser/inboxからも拒否する。worker/plannerの既存禁止も維持する。解除・破棄はこのoperationの専用の権限ある結末だけを使う。通常のready-task reviseではin_progressをsubmittedへ戻せない。

## goal reviewとgoal close（予定・未実装）

ADR-t1521-2決定5は、ADR-0047決定43のgoal reviewの起動・achieved適用と、決定8の`goal close --verdict achieved`の拒否条件を改める。所属taskの`completed` / `canceled`に加えて、`replaced`の置換元は**置換先を再帰的に辿った全ての葉が`completed`か`canceled`になった場合だけ**終了条件上の解決済みとして扱う。未終端の葉、再計画で保留中の子が残れば起動もachievedでのcloseも待つ。置換先が空・欠落・循環している記録は解決済みとせず、復旧へ渡す。置換元のstatusは`replaced`のままで、`completed`へ変えず、goal reviewの「completedが1件以上」の件数に加えない。実際の子など所属taskのcompletedだけを数える。cancelされた子も含む終了条件の充足はgoal acceptanceの達成証明ではない。後続のclaimは上の契約どおり必要な子全てのcompletedを要求し、この終了条件を依存充足には使わない。

goal reviewの入力には元条件の対応表、置換の系統とsnapshot参照、子の着地commit・receipt・検証/review/e2eの根拠、cancelや権限ある削除/破棄の理由を載せる。元runの未着地snapshotを達成証拠にせず、元goalのacceptanceを子の成果で照合する。足りなければ通常のgaps/askへ進む。既存のdraft・未完task・open ask・follow-up所属判断・acceptance版などの起動/close条件は維持する（[Goal review](goal-review.md)）。

fingerprintには既存の入力に加えて、置換元と再置換先のID・status・version、置換relation/元条件対応のversionを含める。goal reviewのachieved、`approve_goal`のachieved answer、人/inboxによる`goal close --verdict achieved`は、同じtransaction内で置換の系統・全葉の終了・条件対応・所属と既存のclose条件を再検査する。reviewのverdictは起動時のfingerprintとも一致させ、起動後の再置換・子の復帰・条件変更なら適用せず取り直す。再起動でも同じ永続記録を読む。実装責務(a)(c)はこの共通判定、入力/fingerprintとclose経路の配線を含み、再置換が終わるまで待つこと、全子が終われば起動/closeできること、未達をcompletedにしないこと、cancelで達成を捏造しないこと、並行した置換で古いachievedを適用しないことをunit/integrationで検証する。

## ask・request・再起動・継続と撤回

askの行の持ち主（元runかrequest）を付け替えない。operationは関連askと未配送answerを参照し、保留中の旧workerへの配送を止める。worker_questionのanswerは元条件の判断材料としてplannerに参照で渡すが、旧workerへのask_deliveredとは記録しない。planner自身の問いは同じrequestのplannerだけが開く。既存approve_landingのanswerも保留中は着地に使わない。旧計画を対象にしたland/cancelの答えを新計画の承認に読み替えず、版違いを記録し必要なら新しい問いを作る。

継続時は未配送answerと元の指摘を同じrunの新generationのbackground wrapperへ一度だけ送る。置換確定時は適用できなくなった旧runのaskを理由と置換先参照付きでruntimeがanswer/closeし、既存の人の答えは上書きしない。引き継ぐ判断はsnapshot/子promptに参照する。approve_plan/planner_questionのanswerもproposal/request versionと適用の結末を再検査し、古いanswerで適用しない。

requestは既存のopen/proposed/declined/exhaustedを維持する。置換起点のrequestはoperationとsnapshotを持ち、openは計画待ち、submitでproposed。ただしproposedは適用済みを意味しない。operationのapplied/continuing_after_park/withdrawnが実際の結末で、inboxに依頼・診断・保留・proposal・適用か継続を結んで知らせる。declined/exhausted、proposal取り下げ、job失敗は旧成果を保留したblockedへ戻し、黙ってcancel/readyにしない。修正は同じoperation/requestの案をreviseし、別requestが必要な言い直しは旧operationの閉じ方を先に記録する。

継続・案の撤回は、子がまだclaimされていないこととoperation versionを確認して、未適用proposalの子をclaim不可のまま閉じ、元taskの条件/依存を保持したままguardを新generationの継続へ移す。旧background wrapperの終了を確認してから一つだけ新wrapperを起動する。同じprovider会話が使えなければsnapshotから新sessionを立てる。復元や起動の失敗はblockedのまま。新receipt・validation・review・必要なe2e・integrateを通し、古いpassを新HEADに流用しない。継続でも既存のrevise/resumeの数はリセットしない。applied後の撤回は元をreadyに戻す操作ではなく、新たな条件付き再計画とする。

再起動/adoptは永続状態を照合する: quiescingは書き手の終了待ちから、snapshottingはmanifest/ref/dirの照合から、parked以後はsnapshotとrequestの欠けの補修から再開する。planner/jobの孤児とleaseを確認し、同じrequestで二つのplannerを立てない。applyingでDB未確定なら再検査し直し、appliedなら後処理だけを行う。旧receiptや旧終了eventは履歴として取り込んでも状態を逆行させない。再計画を知らない古いruntimeがguardを迂回するのを防ぐ非互換schema/起動の柵は実装taskが設ける。

## Spikeとの共通基盤・実装の責務

goal 96のADR-t1487-1と、task 1395で実装した永続request・runtime planner・plan review・ask/answer・起動回数とadoptを共有する。Spikeのcompletedからの依頼は結果による次の計画で、未達runの保留/置換とはoriginと成果の状態が違う。同じgoalでSpike起点とtask置換起点の再計画planner/適用は一つずつに直列化し、後の案は前の結末とgoal versionを読んで作る。workerはslotを返してplannerを待ち、plannerは案をsubmitして終わり、子の実行を枠を持ったまま待たない。調査の周期/同時数の柵はSpikeの契約を使い、再計画の回数と混ぜない。

後続の実装責務は、(a) queueの状態/version/lease/claim/権限/原子的置換とmigration、(b) background停止・snapshot・adopt/receipt柵・復元、(c) 診断とplanner/request/proposal/plan reviewの配線、(d) CLIの追跡・plugin手順・評価に分ける。各実装taskはその変更に対応するdesignの予定の節を実装済みへ更新する。本taskはruntime/schema/CLI/pluginを実装しない。

適用前のtestは原因別判断をunit、境界をintegrationで確認する。二つのsupervisor、exec/restart、各保存段の失敗、重複request/proposal/answer/適用、遅延receipt・旧worker、孤児process、review/e2e/integrateとclaimの競合、全後続と再置換DAG、通常edit/cancel/readyの拒否、snapshotのdirty/index/untracked/Git操作復元を含める。子の検証・review・e2e/integrateの関門を実際に通ることも確認する。workerはhostの実queueやe2eを操作せず、hostのe2eは通常どおりruntimeの関門が行う。

## 回数・抑制・上限と観測

CLIのtask/run/request/proposal/eventの読み取りにoperation、generation、診断窓と原因、保留時刻、snapshot、元条件対応、全後続のbefore/after、引き継ぎ先、判断actor/authority、結末を結ぶ。statsは総turnと原因別turn、review verdict、revise/resume（衝突/e2e/その他）、診断・保留・適用・継続・撤回・失敗の件数と時間を別に集計する。原因未分類はunlabeledとし、reviewの理由件数とreview回数を混ぜない。一つのturnに原因が複数なら主因ごとの件数は一つ、副因は別の非加算表示にする。

候補検知の初期値は同じ原因の非pass reviewが連続3回か、同じ診断上の残作業が2回続けて減らないこと。総turnは参考に表示するだけで自動停止/分割に使わない。自動診断は同じrunの同じHEAD/条件/根拠event窓で一回だけ、継続を選んだ後は新しい根拠か進捗の変化まで抑制する。手動依頼は抑制を越えても重複操作は作らない。

一つの元taskから再置換先までを含む系統に自動の保留・再計画を3回までとする（pauseの柵の確定で一回、保存や起動のretryは同じ回）。適用・撤回・継続でも数を戻さず、再起動でも永続記録で数える。到達したら根拠と今の成果を保持して人のscope/recovery_failed判断へ上げ、上限を超える自動分割をしない。権限ある追加一回の判断は対象系統・理由・回数を記録する。requestのplanner起動は既存の一件3回、plan review/revise/resume・Spikeの上限も独立に守る。抑制/上限の記録から候補数と実際の保留数を分けて評価する。設定の実装ではこれらの値を文書と一致させ、固定バイナリが読めるまでdagq.tomlへ新keyを足さない。

## 歴史的な観測窓

#1405、run `42637110`の例は動き続けるrunの最新総数ではなく、**event 70011（2026-10-03T00:47:54Z、12番目のturn_finished）までを含み、70018より前で締めた窓**である。task 1522の基準値もこの同じ締めと定義を使い、後の数を混ぜない。根拠はこのtaskのcontextに渡されたeventの観測で、本taskではqueueを再測定しない。

| この窓の数 | 定義と根拠 |
| --- | --- |
| turn_started 12 / turn_finished 12（12 turn） | 同runのそれぞれのevent件数。startedは送ったturn、finishedは終わったturn。実行中はstartedだけに数える。終端は70011 |
| review 10回 | review_finishedのevent件数。理由の数でもjob起動数でもない |
| 差し戻し8回 = concern 3 + revise 5 | pass以外のverdict件数。concern: 65320・65818・69064、revise: 65751・68811・68981・69528・69651 |
| pass 2回 | 69789・69907。passの後も衝突やe2e対応はあり、taskの着地と同義ではない |

proposal 548の提出時点は別窓で、event 70025（2026-10-03T00:50:05Z）までならturn_started 13 / turn_finished 12、review_finished 11（非pass 9、pass 2）。追加の非passは70018（11回目のreview_finished、revise）。この別窓の13 turnやreview 11を歴史的基準値に加えない。

この事例を「12 turnなので分割」とは判断しない。concern/reviseの指摘の重なり、条件ごとの残作業、passしたHEADを診断し、衝突の解消は既存のprecheck/resume/継承retryを比較し、e2eの失敗は対象commitのlogでコード修正かhost復旧を分ける。渡された観測に衝突/e2eのevent ID別件数は無いので推測してreviewの8回やturnの12回へ加えない。原因別の測定で衝突/e2e eventと対応を集計するときも同じ締めのevent列を使い、この窓の基準値を変えない。分割する案でも未達の指摘と必要なe2eを子に引き継ぎ、元成果を保存してから新しい関門を通す。
