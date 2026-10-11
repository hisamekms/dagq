---
id: adr-t2257-1
type: adr
title: containerのruntimeは言語・道具・外への宛先・hostで流すコマンド・cache・資源の値をruntimeに固定せず、projectの値はdagq.toml、hostの値はhost.tomlで受ける（ADR-t2113-1決定6・ADR-t2113-3決定3・ADR-t2114-1決定2・5・ADR-t2114-4決定1〜5をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t2113-1 decision 6
  - adr-t2113-3 decision 3
  - adr-t2114-1 decision 2
  - adr-t2114-1 decision 5
  - adr-t2114-4 decision 1
  - adr-t2114-4 decision 2
  - adr-t2114-4 decision 3
  - adr-t2114-4 decision 4
  - adr-t2114-4 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - security
  - conventions
related:
  - adr-t2113-1
  - adr-t2113-2
  - adr-t2113-3
  - adr-t2114-1
  - adr-t2114-4
  - adr-t2086-1
  - adr-t1233-2
  - adr-t1453-2
  - development-task-registration
  - design-security
  - design-queue-service
  - design-supervisor-lifecycle-run-environment
---

# ADR-t2257-1: containerのruntimeはprojectとhostに固有の値を設定で受ける

## Context

dagqは外部のprojectで使うもので、このrepositoryはdogfoodingしているだけである。
containerの実行側を決めたADR（[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)・[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)・[ADR-t2114-1](2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)・[ADR-t2114-4](2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)）とその実装の前提は、dagq自身の開発の事情をruntimeの決まりにしていた。
crates.ioだけの宛先、e2eとmacOSに固有のtestをhostに置くこと、常に起動するsccache、dagqのbuildで測ったmachineの資源と、実装が前提にしたRustのtoolchainのimageである。
これらをruntimeに固定すると、別の言語やregistryのprojectは使えないか、要らない道具と宛先を抱える。

## Decision

1. **原則。**
   runtimeは、dagq自身の開発に要る言語・道具（Rust・cargo・cargo-nextest・sccacheなど）・package registry（crates.ioなど）・hostに残すコマンド（e2e・OSに固有のtest）・compileのcache・資源の値を、runtimeの決まりや既定値として固定しない。
   runtimeは仕組みと、どのprojectにも通じる既定（無ければ使わない・拒む側）を持つ。
   projectに固有の値は`dagq.toml`（commitされ、cloneした全員が継ぐ）、hostに固有の値は`host.toml`で与える。
   このrepositoryの値は、その設定のdogfoodingの値として`dagq.toml`と文書（`host.toml`の値はdocs）に置く。
2. **image。**
   runtimeがbuildするのは、runtimeの要るものだけのbase（Claude Code・Linuxのdagqのclient・containerの中の転送・非rootのuser）である。
   projectの言語と道具は、`dagq.toml`が名指すrepositoryの中のContainerfile（baseをbuildのARGで受けてFROMにする）で足し、名指さなければbaseだけを使う。
   tagはbaseの識別子・projectのContainerfileの中身・`dagq.toml`が入力として挙げるファイル（このrepositoryなら`rust-toolchain.toml`）から決める。
   Containerfileと入力は制御側のmainのcheckoutから読み、runのcloneからは読まない（着地前のworkerの変更でimageを変えさせない）。
   Linuxのdagqのclientはinstallしたdagqと同じ版のものを入手し、dagqのsourceのcheckoutがprojectであることを前提にしない。
   入手の手段は実装がdesignに書き、入手できなければcontainerを使えないものとしてfail closedにする。
3. **外への宛先（ADR-t2114-1決定2・5をamends）。**
   runtimeが組み込みで持つ宛先は、runtime自身が要るqueue serviceと`api.anthropic.com`だけである。
   package registryの`CONNECT`のallowlistは`dagq.toml`がhostを挙げて与え（既定は空で、どのregistryにも届かない）、制御側のmainのcheckoutから読む。
   各hostを足す理由は`dagq.toml`のコメントに書く。
   上流のgitのhost（runのcloneの元のremoteのhost）はallowlistに書いても拒み、実行側がpushの経路を持たないこと（[ADR-t2113-2](2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)決定3）を保つ。
   GitHubのCIの読み取りを宛先にしないこと（ADR-t2114-1決定3）も変えない。
   integrateのverificationのtokenが届くのはこのallowlistだけである。
   containerの中のpackage managerには標準のproxyのenv（`HTTPS_PROXY`など）で転送を向け、cargoに固有の設定をruntimeに置かない。
   このrepositoryはcrates.ioとcargoの依存の取得に要るhostを`dagq.toml`に書く。
4. **hostで流すもの（ADR-t2113-3決定3をamends）。**
   runtimeは「e2e」「macOSに固有のtest」を知らない。
   containerのverificationは検証コマンドを全部containerで流し、runtimeが自分でhostに回さない。
   hostで流すのは`dagq.toml`が宣言したものだけである。
   - (a) reviewのpassの後に制御側がhostで流すコマンド（今の`[e2e]`の形）。
     runのコードを隔離の外で走らせるので、走ったことをeventに残し、`doctor`・`status`のenforcementに出す。
   - (b) containerのverificationの前に、制御側がmainのcheckoutからhostで流してrun dirに材料を置く準備のコマンド（runのコードは走らせない。このrepositoryでは着地のITの対応表の取得）。
   containerで流せないOSに固有のtestをどこで流すか（CIに任せるなど）はprojectの決定で、このrepositoryのmacOSに固有のtestの場所はまだ決まっておらず、別のADRが決める。
   制御側（supervisor・queue service・queueのbroker・inbox・planner）をhostに置くことは変えない。
5. **cache（ADR-t2114-4決定1〜5をamends）。**
   cacheを書き手の信頼で分ける境界（決定4: workerのcacheは信頼せずworker・resumeのcontainerどうしだけが共有し、integrateのverificationとhostの信頼するcacheはmountせず、verificationは保護されたcacheが無ければcacheなし）は、言語に依らない規則として保つ。
   workerのcacheに何を置くか（containerの中のdirの一覧。cargoのregistry・sccacheのdir・npmのcacheなど）は`dagq.toml`が与え、既定は無し。
   workerのcacheはqueue（project）ごとに分け、別のprojectのworkerと共有しない。
   sccacheのserverをcontainerの中に起動する仕組み（決定1〜3・5）は、`[run.env]`がsccacheを名指すとき（hostのsccacheの扱いと同じ条件）だけ働く任意の機能である。
   名指さないprojectではserverを起動せず、`RUSTC_WRAPPER`も扱わない。
6. **資源（ADR-t2113-1決定6をamends）。**
   dagq専用のmachineはhostに1つで複数のprojectが共有するので、machineのCPU・メモリとcontainerごとのmemory / cpu / pidsの上限は`host.toml`の値にする。
   runtimeの既定は、dagqのbuildで測った値ではなく、どのprojectにも通じる控えめな値にする（値と根拠は定義のそばのdoc comment）。
   [Spike](../plans/container-runtime-spike.md)の(c)のdagqのbuildで測った通る最小の値は、このrepositoryを動かすhostの`host.toml`の推奨値としてdocsに書き、runtimeの既定にしない。
   containerのhardening（非root・読み取り専用のroot filesystem・capabilityの削除・特権の昇格の禁止・Podman / Dockerのsocketを渡さないこと）は変えない。

## 既存の決定との関係

- ADR-t2113-1決定6: 「machineの資源は通る最小を測って決め、決めた値はdesignに書く」を、`host.toml`の値と控えめな既定に変える。hardeningは保つ。
- ADR-t2113-3決定3: 「e2e・macOSに固有のtestをhostに置く」を、`dagq.toml`が宣言したコマンドだけをhostで流す形に変える。制御側をhostに置くことは保つ。
- ADR-t2114-1決定2: crates.ioの宛先を、`dagq.toml`が与えるregistryのallowlistに変える。queue serviceと`api.anthropic.com`、tokenの検証と差し替え、拒んだ要求をeventに残すことは保つ。
- ADR-t2114-1決定5: egressのallowlistの中身を「3つ」から「組み込みの2つと`dagq.toml`のallowlist」に変える。brokerが唯一の出口であることは保つ。
- ADR-t2114-4決定1〜3・5: sccacheのserverの仕組みを、`[run.env]`がsccacheを名指すときだけ働くものにする。
- ADR-t2114-4決定4: 境界は保ち、workerのcacheの中身を`dagq.toml`から受け、queueごとに分ける。
- ADR-t2114-4を置き換えずamendsにする理由: 決定1〜3・5はsccacheの仕組みの中身を保って働く条件を足すだけ、決定4は境界を保って中身の出どころを変えるだけ、決定6・7は変えないので、決定の大半を変えるもの（[文書の規則](../development/documents.md)の「ADR」）に当たらない。
- amendsしない決定の読み方: ADR-t2114-4決定7の「brokerの宛先は3つのまま」は、sccacheを宛先に足さないことの意味で保ち、宛先の中身はこのADRの決定3で読む。ADR-t2113-1決定5の「最小の資源でinit」の資源は、決定6の`host.toml`の値か控えめな既定である。

元のADRの本文は書き換えない。

## Alternatives

- **dagqの値をruntimeの既定にし、他のprojectに上書きさせる**: 上書きを忘れたprojectで、Rustの道具とcrates.ioへの経路が黙って入り、宛先の既定が開く側になる。
- **registryのallowlistを`host.toml`に置く**: registryはprojectの依存で決まり、cloneした全員が同じ値を要る。
- **imageを設定で丸ごと差し替え、baseを持たせない**: runtimeの要るClaude Code・client・転送・非rootのuserを、各projectが正しく入れる必要が出る。

## Consequences

- runtimeの汎用性は、runtimeのtaskの登録とplan reviewで確かめる（[taskの登録](../development/task-registration.md)の「runtimeの汎用性」）。
- containerのimage・宛先・cache・資源・hostで流すコマンドの実装のtaskは、この形を前提に書き直す。
  このrepositoryの値（Containerfile・registryのhost・workerのcacheのdir・hostで流すコマンド）は`dagq.toml`に、machineの資源の推奨値はdocsに置く。
- 設定の無いprojectのcontainerは、baseのimage・registryに届かない宛先・cacheなし・控えめな資源で動き、要るものを足すまで依存の取得やbuildが失敗しうる。
  黙って開く側にはならない。
- 新しい設定のkeyをこのrepositoryの`dagq.toml`に足すのは、固定バイナリが読めるようになってからである。
