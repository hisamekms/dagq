---
id: adr-t2113-2
type: adr
title: gitはmountの設計で守る。containerのrunにはhostのgitの共通dirをmountせず独立したcloneを渡し、integrateが制御側でrunのcloneから取り込んで確かめ、runのcloneにはpushの権限と上流の資格情報を置かない
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
owners:
  - hisamekms
tags:
  - runtime
  - security
  - git
related:
  - adr-t2113-1
  - adr-t2113-3
  - adr-t1233-1
  - adr-t728-2
  - adr-t827-2
  - design-security
  - design-supervisor-lifecycle-integrate
---

# ADR-t2113-2: gitはmountの設計で守り、containerのrunには独立したcloneを渡す

## Context

[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)でresource brokerを外すと、gitの扱いを仲介する層が無くなる。
今のrunはmain checkoutの`git worktree add`で作るworktreeで、worktreeの`.git`ファイルはhostのgitの共通dir（`.git`）の下の`worktrees/<name>`の絶対パスを指し、objects・refs・config・hooksをmain checkoutと全てのrunが共有する。
これをそのままcontainerにmountすると、containerの中のコードが共通dirの`config`（`core.fsmonitor`・`core.hooksPath`など）や`hooks`を書き換え、supervisor・Integrator・人のhostのgitで任意のコードを走らせられる。
他のrunのbranchとmainのrefも書き換えられる。

[ADR-t827-2](2026-09-28-t827-2-broker-transport-run-token-and-workspace-confinement.md)は、共通dirをmountして`config`と`hooks`を読み取り専用で重ねる形（決定5）と、gitがpushを持たず上流の資格情報を置かない形（決定7）をとった。
着地とpushは信頼するIntegratorだけが行う（ADR-t728-2）。
着地はrun branchをrebaseしてverificationを流し、mainへfast-forwardする（[integrate](../design/supervisor-lifecycle/integrate.md)）。

## Decision

1. **containerのrunには独立したcloneを渡す。**
   containerで動くrun（[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)のworker・resume）には、hostのgitの共通dirをmountせず、制御側がrunごとに作った独立したcloneをmountする。
   cloneはhostのobjectsを共有しない形で作る（hostのobjectのファイルとinodeを共有するhard linkも、hostのobjectsを参照するalternatesも使わない）。
   alternatesはhostのobjectsのmountを足し、worktreeとrun dirだけを見せる規則（ADR-t2113-3決定1）を破るうえ、hostの`git gc`が参照中のobjectを消しうるため。
   containerの中のgitの操作（commit・branch・config・hook）は、そのcloneの中だけで閉じる。
   integrateのverificationのcontainerにも共通dirをmountせず、制御側がrebaseの後のcommitから作ったclone（か`.git`の無い作業ファイル）を渡す。
2. **integrateは制御側でrunのcloneから取り込んで確かめる。**
   integrateは、runのcloneからrun branchを制御側のrepositoryへfetchし、取り込んだcommitを今の着地と同じ手順（baseとの関係・rebase・verification・review）で確かめてから着地させる。
   取り込みはrunのcloneの`config`と`hooks`をhostのgitに解釈させない形で行い、制御側の設定だけで走らせる。
   cloneのpathからの素のlocalのfetchは、clone側で`git-upload-pack`がcloneの`config`を読んで走るのでこの要件を満たさない。
   満たす形の例は、containerの中でrun branchの`git bundle`を作り、制御側がそのbundleからfetchする形で、どの形にするかは実装が決める。
   runのcloneの他のrefは取り込まない。
3. **runのcloneにpushの権限と上流の資格情報を置かない（ADR-t827-2決定7のうちgitに残る不変条件の引き継ぎ）。**
   runのcloneのremoteは上流を指さず、credential helperと上流の資格情報（token・SSHの鍵）をcloneにもcontainerにも置かない。
   containerから上流へ届く経路を持たない（外への経路はqueueのbrokerだけで、その宛先に上流のgitのpushは入れない）。
   着地とpushはIntegratorだけが行う（ADR-t728-2）。
4. **次善の形は、推奨の形の費用が実測で受け入れられないときだけ採る。**
   次善は、共通dirを同じ絶対パスでmountし、`config`と`hooks`を読み取り専用で重ねる形（ADR-t827-2決定5の重ね方）である。
   この形では、containerの中のコードが他のrunのbranch・mainのref・共有のobjectsを書き換えうることが残るので、採るときはそれを既知の制限として記録し、integrateが取り込むcommitをrun branchに限って確かめる。
   共通dirのmountはworktreeとrun dirだけを見せる規則を越えるので、採るときはADR-t2113-3決定1のamendsも要る。
   どちらにするかは、下の費用の実測で決める。

## 費用の見積もり

2026-10-08のこの repositoryは、packが約27 MiB、loose objectが約188 MiB（未packの22,166個）、追跡するファイルが1,402個で作業ファイルが約33 MBである。

- cloneの作成: pathを渡す`git clone --no-hardlinks`はobjectsをそのまま複製するので、今は約215 MiB（loose約188 MiBとpack約27 MiB）と作業ファイル（約33 MB）をrunごとに書く。
  `file://`のURLで渡すclone（packを作って送る）なら、objectsはpack相当の数十MiBになり、packを作る時間が足される。
  hostの同じdisk上で、どちらも数秒から十数秒の見込み。
  hostのgitの共通dirを定期的にpackすれば、どちらの形でも複製は小さくなる。
- fetch: run branchの差分のobjectだけを運ぶので、1 runの変更の大きさ（多くは数百KB以下）に比例し、数秒以内の見込み。
- 比べる相手: 今のworktreeも作業ファイルのcheckoutは同じだけ要り、runのbuildの`target/`（GB単位）の方がdiskを大きく使う。

これは文書の範囲の見積もりで、実測（cloneの作り方ごとの時間とdisk、同時に走るrunの数での合計）はworkerのcontainer化を実装するgoalで行う。

## Alternatives

- **共通dirをmountし`config`と`hooks`だけを読み取り専用で重ねる（次善）**: hostのコードが走る経路は塞げるが、他のrunとmainのref・共有のobjectsへの書き込みが残り、守る範囲がgitの内部の構造（どのファイルが危ないか）に依存する。
  推奨の形は共通dirを見せないので、gitの版が増やす設定やファイルにも依存しない。
- **runにgitを渡さない（作業ファイルだけを見せ、commitは制御側が作る）**: workerがcommitとbranchの履歴を使えず、promptとreceiptの約束（runのbranchにcommitする）を作り直す必要がある。
- **resource brokerのgitのopで仲介する**: [ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)の退けた案（brokerをrunごとに作り直す）と同じ理由で採らない。

## Consequences

- workerのcontainer化の実装は、runのworktreeの作り方（`git worktree add`）をcontainerのrunではcloneに替え、integrateにcloneからの取り込みを足す。
  hostで動く間のrunはworktreeのままでよい。
- runのcloneは共通dirを持たないので、runの中のgitのhook（`.githooks`）とmain checkoutの設定はcloneに効かない。
  runに要る設定は、制御側がcloneを作るときに入れる。
- cloneの作成と後始末の費用がrunごとに増える。
  後始末はworktreeと同じ契機（runの終わりと着地）で行う。
