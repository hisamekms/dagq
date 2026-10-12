---
id: adr-t2179-1
type: adr
title: containerのrunのcloneは制御側にとって信頼しないdataで、制御側はcloneでgitもプログラムも走らせず、bundleと状態のファイルから事実を取り、制御側だけのdirの着地用のworktreeで受け入れたcommitを着地し、tracked の中身が選ぶhookとdriverを走らせない（ADR-t2113-2決定2をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t2113-2 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - security
  - git
related:
  - adr-t2113-2
  - adr-t2113-3
  - adr-t2173-1
  - design-security
  - design-container-runtime
  - design-supervisor-lifecycle-integrate
---

# ADR-t2179-1: 制御側はcontainerのrunのcloneでgitを走らせず、bundleと状態のファイルから事実を取り、自分の着地用のworktreeで着地する

## Context

[ADR-t2113-2](2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)決定2は、integrateがrunのcloneからrun branchを取り込んで確かめるとした。
ところがreceiptの検証はcloneの`HEAD`と状態を読み、祖先と差分を制御側のobjectsで判定する。取り込みが着地だけだと、新しいcommitは検証で落ちる。
制御側の読み取り（`git -C <clone>`）はcloneの`config`を解釈するので、`core.fsmonitor`・`core.hooksPath`・`filter.*.process`・`include`などをworkerが書けば、取り込みより前にhostでプログラムが走る。
`GIT_OPTIONAL_LOCKS=0`や`-c`での上書きは守りにならない（repositoryのconfigはenvで無効にできず、危ない設定の列挙はgitの版で増える）。
取り込んだcommitの中身（trackedの`.githooks`・`.gitattributes`）も、制御側のrepositoryの設定（例: `core.hooksPath=.githooks`）から選ばれてhostで走りうる。
reviewの間もworkerのsessionは残るので、run dirの下に置いた作業場所はworkerが書き換えられる。

## Decision

1. **cloneは信頼しないdataで、制御側はそこでgitもプログラムも走らせない。**
   workerが触った後のcloneを、制御側（supervisor・Integrator・復旧job・reviewのjobとprogram・e2e・cleanup・rebind）はrepositoryとするhostのgitを一切走らせず、cwdをcloneにしたプログラム（起動時にcwdでgitを走らせるagentのCLIを含む）も起動しない。
   cloneを作る時点は制御側が入れた中身だけなので、作る操作は制御側のgitで行ってよい。
2. **runの事実はrun branchのbundleとrunの状態のファイルから得る。**
   どちらもcontainerの中のexecutorがturnの終わりにrun dirの決まったpathに書く。bundleはrun branch 1本だけでbase commitを前提にし、新しいcommitが無ければ作らない。
   状態のファイルはHEADが指すbranch・HEADのcommit・`git status --porcelain --untracked-files=all`の出力・rebaseが途中かを持つ。綴りと形は定義のそばのdoc commentが持つ。
3. **取り込みはheadが要るたびに、制御側の設定で明示のrefspecで行う。**
   bundleを制御側だけの写しにしてから、`bundle list-heads`でheadが1本で名前がrun branchであることを確かめ、制御側の`refs/heads/<run branch>`だけを強制で更新するfetch（tagなし・FETCH_HEADなし・submoduleを辿らない・`transfer.fsckObjects`）で取り込む。
   cloneのpathからの素のlocalのfetchはしない。
4. **receiptの検証は、取り込んだcommitを制御側のobjectsで判定し、branchと状態は状態のファイルから読む。**
   祖先・merge base・差分・pathsの検査・e2eの要否は今の検査のまま。状態のファイルのHEADとbundleのheadの食い違い、bundleか状態のファイルの欠け・壊れ・前提のcommitの欠けは、commitの食い違いとしてsessionに返し、制御側のgitでcloneを読みに行かない。
   状態のファイルはworkerが偽りうるが、偽って得るのは「commitしていない変更が着地しない」ことだけで、着地する中身は取り込んだcommitを制御側が確かめるので、安全はこのファイルに依らない。
5. **着地は制御側だけのdirの着地用のworktreeで、検証が受け入れたcommitに固定して行う。**
   着地用のworktreeはqueueのdirの下の`landing/<run>`（0700、制御側の所有）に、検証が受け入れたcommitをdetachedで置き、今と同じ手順（baseとの関係・途中のrebaseのabort・receiptとheadの照合・cleanの確かめ・rebase・migrationの番号の振り直し・verification）で着地する。reviewのjobとprogram・e2eもここで動く。
   run dir・cloneの親のdir・verificationのcloneの親のdirのどれにも含まれず、どのmachineとcontainerのmountにも入らない（[ADR-t2173-1](2026-10-12-t2173-1-machine-mounts-only-run-and-clone-dirs-and-keeps-build-output-and-worker-cache-in-named-volumes.md)決定1）。
   既に在るqueueのdirと条件を満たすlandingのdirは拒まない。着地用のworktreeのpathに登録のない物が在る、landingのdirがsymlinkかdirでない、制御側の所有でないか group・otherが書けるときは作らずに、着地せず人を待つ。
   reviewの間にworkerがbundle・状態のファイル・cloneを書き換えても、着地する中身は変わらない。次の検証（resume・reviseの後）が前の着地用のworktreeを消して作り直す。
6. **mainはbundleでcloneに渡す。**
   cloneは上流を指さず制御側のrefsも共有しないので、rebaseを求めうる要求（resume・revise・衝突）をsessionに渡す前に、制御側がbase commitを前提にした今のmainのbundleをrun dirに書き、executorが次のturnの始めにcloneの決まったrefへ取り込む。差し戻しの理由とresumeのpromptはcontainerのrunではそのrefを名指す。
7. **片付けはworktreeと同じ契機（runの終わりと着地）で、gitを使わずsymlinkを辿らずに行う。**
   cloneのdir、制御側のrun branch、着地用のworktree、run dirのbundleと状態のファイルを消す。cloneのdirとrun dirのファイルはsymlinkを辿らずに消し、`git worktree repair`とbuildの出力の片付けはcloneに走らせない。
8. **containerのrunのcommitを扱う制御側のgitは、trackedの中身が選ぶhookとdriverを走らせない。**
   bundleの取り込み・cloneと着地用のworktreeの作成・rebaseとabort・番号の振り直しのcommit・祖先と差分の検査・mainのcheckoutのfast-forward・landing recheckと衝突の事前の確かめ・片付けは、`core.hooksPath`をhookの無いpathに向け、制御側のconfigの全てのscopeが定義するfilter・diffのtextconvとcommand・mergeのdriverをプログラムを走らせないものに上書きする（command scopeは他の全てのscopeに勝つ）。`--no-verify`には頼らない。
   上書きが効いていることをgitが読むconfigで確かめ、確かめられなければ着地しない。reviewのjobとprogram・e2eなど着地用のworktreeで制御側が起こすプロセスにも同じ上書きをenvで渡す。
   trackedのコードを明示に走らせる経路（[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定3のe2eの例外とcontainerのverification）はこの決定の外である。hostのrunの着地は今のまま。
9. **run dirのファイルはsymlinkを辿らずに読み書きする。**
   制御側が書くmainのbundleは、在る物をsymlinkを辿らずに消してから新しく作り、読むbundleと状態のファイルはsymlinkでない通常のファイルだけを開く。symlinkや通常のファイルでない物は壊れたbundleと同じ差し戻しにする。

## Alternatives

- **取り込みをintegrateだけにする（ADR-t2113-2決定2の字面）**: 新しいcommitがreceiptの検証で落ち、検証が`-C <clone>`で読めばcloneのconfigがhostで解釈される。
- **危ない設定を列挙して`-c`で消し、cloneでgitを読む**: repositoryのconfigはenvで無効にできず、includeとgitの版が増やす設定に追いつけない。
- **着地用のworktreeをrun dirの下に置く**: reviewの間とresumeの後にworkerが`.git`とファイルを書き換えられる。
- **`--attr-source`でattributesを空のtreeから読む**: Git 2.40以降で、このhostのGit 2.39に無い。

## Consequences

- containerのrunの着地では制御側のhook（このrepositoryではADRの索引の作り直し）も走らない。filterに頼るproject（例 git-lfs）では、着地用のworktreeとmainのcheckoutにfilterをかけない中身が出る。
- 制御側のgitの各操作の前にconfigの読み取りが足される。
- containerのrunのrun dirに、executorが書くbundleと状態のファイル、制御側が書くmainのbundleが増える。executor（containerのworker）はこの形に従って書き、読む。
