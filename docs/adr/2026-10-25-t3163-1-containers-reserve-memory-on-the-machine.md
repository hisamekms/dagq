---
id: adr-t3163-1
type: adr
title: dagq専用のmachineで同時に動くcontainer（workerのturnとverification）は、起動の前にhost全体のlockの中で自分のmemoryの上限をmachineのメモリから予約し、入らなければ起動せずに待つ。上限を重ねる形（overcommit）は今の材料では採らない
status: accepted
created: 2026-10-25
updated: 2026-10-25
accepted_on: 2026-10-25
owners:
  - hisamekms
tags:
  - runtime
  - security
  - performance
related:
  - adr-t2257-1
  - adr-t2113-1
  - adr-t2113-3
  - adr-t2173-1
  - plan-build-memory
  - plan-container-oom-detection
  - design-container-runtime
  - design-supervisor-lifecycle-claim-hold
---

# ADR-t3163-1: containerは起動の前にmachineのメモリから自分の上限を予約し、入らなければ待つ

## Context

workerのturn（resumeを含む）とintegrateのverificationのcontainerは、hostに1つで全てのprojectが共有するdagq専用のmachineで動く（[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)決定4、[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1）。
machineのメモリとcontainerごとのmemoryの上限は`host.toml`の値である（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定6）。
containerの上限はcgroupの上限で予約ではないので、上限の合計がmachineを超えればpeakが重なったときにmachine全体のOOM killerが働く。
machine全体のOOM killerは上限を守っているcontainerのprocessも殺しうる。

- 測った値（[build・testのpeakのメモリ](../plans/build-memory.md)）: このrepositoryの1本の`cargo test --no-run`の合計のpeakは7,208 MiB（約7.0 GiB、Claude Codeを足して約7.3 GiB）で、推奨の値（machine 10 GiB・containerの上限8 GiB）では上限いっぱいのcontainerは同時に1本しか入らない。
  測った下げる手では2本は入らない。
- turnのcontainerはbuildの間だけでなくClaude Codeが考えている間も続き、turnの終わりに消える。
- `dagq.toml`の`[supervisor]`の`parallel`はqueueごとで、machineを共有する別のprojectのrunを数えない。
- containerの中のOOM killを確かな事実として読めるか、containerの上限とmachine全体を分けられるかは、[OOMの検知のSpike](../plans/container-oom-detection.md)の「全体の判定」のとおり未解決である。
  OOMの事態の記録（M1・M5）はkillもその区別も言わず、区別できる方法（M2・M4）の成否は人かinboxが流す実物の確かめ（C1〜C5、goal 184のnote）で決まる。

## Decision

1. **予約（(A)）。**
   workerのturnとverificationのcontainerは、machineの`ensure`で自分を使う者にした後、起動の前にhost全体のlock（`ensure`と同じlock）の中で、自分のmemoryの上限を予約する。
   使う者にしてから読むので、止まったmachineを読めずに待ち続けることは無い。
   動いているか予約済みのdagqのcontainer（relayを除く）の上限の合計に自分の上限を足して予算を超えるなら、起動せずに待つ。
   予算は、動いているmachineのメモリ（`host.toml`の値ではなくmachineが実際に持つ値。値は作り直したmachineにしか効かないため）から、machineの中の基盤（OSとpodmanの決まった分と、動いているrelayごとの分）を引いた値である。
2. **値の出どころと新しいkeyを足さないこと。**
   予約する量は`host.toml`の`[container]`のmemoryの上限で、machineのメモリはmachineから読み、`host.toml`に新しいkeyを足さない。
   基盤の分（決まった分とrelay 1本あたりの分）はruntime自身の部品の大きさでprojectに依らないので、runtimeが持つ（値と根拠は定義のそばのdoc comment）。
   予約の量をprojectが下げる手は上限そのものを下げることで、上限より小さく予約するkey（重ねる倍率など）は持たない（決定5）。
3. **数え方はfail closed。**
   予約は持ち主（runのturnかverification）とともにlockの下に記録する（machineの使う者と同じ持ち主の考え方）。
   - turnの予約はそのturnの1つのcontainerに結び、containerを消したときか持ち主のrunが終わったときに外す。
     lockを取るたびに、起動した後のcontainerがmachineに無いturnの予約も外すので、持ち主が生きたまま残ったturnの予約で全体が止まらない。
   - verificationの予約はコマンドごとのcontainerに結ばず、verificationの片付けのときか持ち主のverificationが終わったときにだけ外す。
     verificationはコマンドごとに`--rm`のcontainerを起動するので、コマンドの間はcontainerが無くても予約を持ち、containerが無いことでは外さない。
     外せば、コマンドの間に別の待ち手が同じ予算を取り、次のコマンドのcontainerと合わせて上限の合計がmachineを超える。
   予約の持ち主のcontainerはその予約の中で数え、二重に数えない。
   記録の無いdagqのcontainer（relayを除く）がmachineに在れば、その上限で数える。
   machineの中を読めないときは空いているとせず、起動しない。
   1本の上限が予算を超える設定（基盤の分がmachineのメモリと上限の差より大きい場合を含む）は待っても入らないので、待たずにcontainerを使えないと理由つきで答え、hostに戻さない（[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)の境界と、container runtimeのfail closedの境界のまま）。
4. **待たせ方。**
   待つのはlockの外で、lockは判定と記録の間だけ持つ（`ensure`とrelayの見張りのmachineの起動し直しを止めない）。
   待ちが長く続けば（時間はruntimeのdoc comment）inbox宛てのattentionにし、`status`の待ちの表示だけに頼らない。
   - 新しいclaim: 予算が足りないか、待っている予約が1つでもあれば、containerで動かすworkerの候補をclaimせず、理由つきの控えにする（[claimを控える](../design/supervisor-lifecycle/claim-hold.md)の「workerを動かせないtask」と同じ層）。
   - 走っているrunの次のturn（答え・revise・resume）: wrapperがturnを起動する前に予約を待ち、errorにしない。
     待つ間はturnの時間・idleの見張り・時間切れに数えず、待った区間を記録して`status`で見せる。
   - verification: supervisorは予算の不足では着地を控えず、着地を始める。
     `integrate`は最初の検証のコマンドの前に待ちの列に入って予約を取り、verificationの片付けまで持つ（コマンドごとに外さない）。
     取れるまで待ち、hostで流さない。
     着地を控えると列に入らず、下の順でturnより先にする意味が無くなるため。
   - 待っている予約の順はhost全体で、verificationを走っているrunのturnより先にし、同じ種類の中は待ち始めの順にする。
     verificationはできた仕事の着地で、turnは新しい仕事なので、着地を先にすると進行中のrunが減る。
     projectを問わず同じ列に並べるので、あるprojectのqueueの`parallel`が大きくても別のprojectの待ちを追い越さない。
5. **重ねる形（(B)と(C)）は今は採らない。**
   上限を予約にせず重ねる(B)も、verificationだけ予約しworkerのturnは重ねる・重ねる倍率を`host.toml`で持つ(C)も採らない。
6. **ADR-t2257-1決定6との関係。**
   この決定は決定6を変えない追加の決定で、決定6が置いた`host.toml`の値と控えめな既定を使って同時の数を抑える。
   runtimeの既定の値は決定6のとおりどのprojectにも通じる控えめな値のままで、既定のmachineと既定の上限で、基盤の分を引いてもworkerのcontainerとverificationのcontainerが同時に入る大きさにする（値は実装がdoc commentで決める）。
   今の既定（machineのメモリの半分を上限にする形）は基盤の分を引くと2本が入らないので、実装が既定の値を直す。
   値を直すのは決定6の範囲（控えめな既定と、値と根拠をdoc commentに置くこと）の中で、決定6は変えない。

## 既存の決定との関係

このADRはどのADRもamendsも置き換えもせず、次の決定を保ったうえで、containerが同時にmachineのメモリを使う量を抑える決定を足す。

- [ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定6: machineの資源とcontainerごとの上限を`host.toml`の値と控えめな既定にすることを保ち、予約はその値を読んで使う（決定2・6）。
- [ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)決定4〜6: dagq専用のmachine、`ensure`と停止をhost全体のlockで直列にすること、containerのmemory・cpu・pidsの上限とhardeningを保つ。
  予約は同じlockを使い、`ensure`で使う者にした後に取るので、machineの起動と停止の判定を変えない。
- [ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1・3: containerで動かすのはworker・resumeとintegrateのverificationだけで、予約の対象もこの2つである。
  入らないときもhostで動かさない。
- [ADR-t2173-1](2026-10-12-t2173-1-machine-mounts-only-run-and-clone-dirs-and-keeps-build-output-and-worker-cache-in-named-volumes.md)決定1〜4: machineがmountする3種のdir、buildの出力とworkerのcacheをmachineの中のnamed volumeに置くこと、run・verificationごとに見せる相手を分けることを保つ。
  予約はcontainerのmemoryの上限だけを数え、mountとvolumeの作り方・見せる範囲・消す時点を変えない。
  named volumeのdiskの使用は予約に数えない（machineのdiskの値は`host.toml`のまま）。

元のADRの本文は書き換えない。

## 判断の軸ごとの得失

| 軸 | (A) 予約 | (B) 重ねる | (C) verificationだけ予約・turnは重ねる |
| --- | --- | --- | --- |
| 着地の流れ | 同時のcontainerは予算÷上限に減る（推奨の値では1本）。Claude Codeが考えている間もturnが枠を持つので、queueの`parallel`より少ないturnしか同時に進まない | 同時の数は減らない | verificationは待つが、turnは減らない |
| OOMの頻度と差し戻しの無駄 | 上限の合計がmachineを超えないので、machine全体のOOMは起きない。containerの上限によるOOMだけが残り、そのcontainerだけが落ちる | 測った値では2本のcompileが重なればほぼ必ずmachine全体のOOMになる | machine全体のOOM killerは予約したverificationのprocessも殺しうるので、予約が守らない |
| 複数のprojectの公平 | host全体の1つの列で待つ | 先に起動した側が勝ち、落ちる側はどのprojectか決まらない | (B)と同じ |
| fail closedとhostに戻さないこと | 読めなければ起動せず、待つだけでhostに戻さない | OOMを事実として読めない今は、落ちた理由を誤る | (B)と同じ |
| runtimeの汎用性 | 値は`host.toml`とmachineから読み、runtimeが持つのは自分の部品の大きさだけ | 倍率などの値が要る | 倍率と種類ごとの扱いの値が要る |

## Alternatives

- **(B) 上限を重ねる**: 今の材料では前提が立たない。
  OOMのSpikeの`spike_result`はunresolvedで、M1・M5はkillも上限とmachine全体の区別も言わず、M2・M4の成否は実物の確かめで決まる。
  加えて測った値（1本約7.2 GiB、machine 10 GiBに上限8 GiBで同時に1本）では、重ねた2本目のbuildはほぼ必ずOOMになり、OOMを読めない今はworkerがbuildの失敗と取り違えてturnと差し戻しを無駄にする。
- **(C) 一部を重ねる**: 重ねた部分は(B)と同じ前提に立つうえ、machine全体のOOMは重ねた部分のprocessだけを選んで殺すわけではないので、予約した側も守れない。
  OOMの事態を事実として読めない場合（OOM killをverificationの失敗と取り違えない手当てがOOMを読めないとき）には、重ねた部分のOOMは検証の失敗やbuildの失敗と区別できず、扱いを決められない。
- **queueの`parallel`をcontainerのときだけ下げる・runtimeごとの`parallel`のkeyを足す**: queueごとなので、machineを共有する別のprojectを数えず、machineを守れない。
- **起動の時点のmachineの空きメモリを見る**: turnは起動の時点では小さく、後のbuildで大きくなるので、起動の時点の空きは後のpeakを言わない。
- **上限を下げて2本入れる**: 測った下げる手では2本は入らない（[build・testのpeakのメモリ](../plans/build-memory.md)の「推奨」）。

## 重ねる形を見直す条件

次が揃ったときだけ、(B)か(C)の重ねる形をこのADRのamendsで見直す（見直しは別のtaskで、実物の結果を読むplannerが足す）。

- goal 184の実物の確かめ（OOMのSpikeの「結果を goal 184 の note に残す書式」）で、M4か、偽れなさの条件を持てるM2が成立し、containerの上限によるkillとmachine全体のOOMを事実として分けられる。
- containerで動いたrunの記録で、compileのpeakがturnの時間の小さな部分で、重ねたときに重なる見込みが低いと測れる。
- machine全体のOOMのときに殺されるprocessを重ねた側に限る手（予約した側を守る手）を設計できる。

## Consequences

- 実装が要る（runtimeの予約・待ち・控えの理由・待ちの記録と見せ方）。
  実装はこのADRのtaskのreceiptのfollow_upにした。
- このrepositoryの`parallel`は、containerに切り替えるときも3のままにする。
  予約が同時のcontainerを抑えるので`parallel`を下げてもmachineは守られず、reviewとe2eと人の答えを待つ間のrunはcontainerを持たないので、3の枠はcontainerを使わない工程の間も他のrunを進める。
  切り替えの前後の測定で、予約の待ちが着地の時間の大半を占めると分かれば見直す（値は`dagq.toml`が持つ）。
- 推奨の値（machine 10 GiB・上限8 GiB）で1本が入るのは、基盤の分が2 GiBより小さいときだけである。
  入らなければ決定3のとおりcontainerを使えないと答えるので、containerのスモークで基盤の分を確かめ、足りなければmachineを大きくするか上限を下げる。
- 推奨の値ではcontainerは同時に1本で、hostで動かしていたときより同時に進むturnが減る。
  machineを大きくするか上限を下げられるかは、containerのスモークの中の測った値で決める。
- 予約の待ちはturnの時間に数えないので、待っている間のrunを時間切れにせず、runの時間の分析は待ちを除いた値も並べられる。
