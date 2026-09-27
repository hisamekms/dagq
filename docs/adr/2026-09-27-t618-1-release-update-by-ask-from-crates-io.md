---
id: adr-t618-1
type: adr
title: リリースのバイナリで動くsupervisorは、crates.ioのsparse indexで新しいリリースを検知してinboxのaskで知らせ、答えでcargo installからinstallと同じ確認・差し替え・引き継ぎまでを行い、人に聞かない入れ替えはhostの設定のopt-inにする（ADR-0073決定14をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amends:
  - adr-0073 decision 14
owners:
  - hisamekms
tags:
  - runtime
  - operations
  - release
related:
  - adr-0073
  - adr-t598-1
  - adr-t614-1
  - adr-t617-1
  - adr-t618-2
  - design-supervisor-lifecycle-release-update
---

# ADR-t618-1: リリースのバイナリで動くsupervisorは、crates.ioのsparse indexで新しいリリースを検知してinboxのaskで知らせ、答えでcargo installからinstallと同じ確認・差し替え・引き継ぎまでを行い、人に聞かない入れ替えはhostの設定のopt-inにする（ADR-0073決定14をamends）

## Context

[ADR-0073](0073-kind-additions-are-compatible.md)の`dagq install`（決定14）と`up --auto-update`（決定17）はqueueのrepositoryのcheckoutからbuildする仕組みで、[ADR-t614-1](2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)でdagqのソースのrepositoryだけに限られた。外部のprojectは`cargo install dagq`で入れるのが基本で、人は2026-09-27に、外部でもシームレスな更新を要るものとし、新しいリリースを検知したらinboxのaskで知らせ、答えで`cargo install`から入れ替えまでを行い、人に聞かない完全自動はopt-inにすると決めた。

2026-09-27に確かめたこと:

- crates.ioのsparse index（`https://index.crates.io/da/gq/dagq`）は認証なしで読め、versionごとに`vers`と`yanked`を持つJSONの行を返す（今は`0.3.0`）。cargo自身が使う形で、CDNから配られ、形式は文書化されている。crates.ioのAPI（`/api/v1/crates/dagq`）はUser-Agentの指定と1秒1回の上限の方針がある。
- `cargo install`は、buildしたバイナリを置き先のdirectoryの中の一時directoryにcopyしてから`rename`で置き換える（cargo-0.52.0の`ops/cargo_install.rs`で読んだ。今のcargoも同じ形）。走っているsupervisorは前のinodeで動き続けるので殺されない。ただし置き換えの前の確認（ADR-0073決定12）は無く、`.previous`も残らない。
- ADR-0073の引き継ぎ（決定10〜13・15）は、バイナリの出どころを問わず、置き換えたpathのバイナリをsupervisorに自分のpidのままexecさせる。supervisorが引き継ぐpathは自分の`current_exe`（`cargo install`した利用者なら`~/.cargo/bin/dagq`）である。

したがって、今でも人が手で`cargo install --locked dagq`を打ってから`dagq up`を打てば、`up`がbuild識別子の違いで引き継がせる（決定15）。足りないのは検知と、確認と戻しの効く入れ替えを1つの答えで行う経路である。

## Decision

1. **対象はリリースのビルドで動くsupervisorだけ。** supervisorは自分のbuild識別子が`X.Y.Z`だけ（ADR-0073決定2のリリース）のときに新しいリリースを調べる。`-dev`のビルド（dagqの開発。ADR-0073決定17の自動更新が受け持つ）では調べない。これはADR-t614-1の「dagqのソースか」の判定とは独立で、バイナリ自身の名乗りだけで決める。
2. **検知はcrates.ioのsparse indexで、1日1回と起動時に行う。** yankedでなくpre-releaseでないversionのうち最大のものを最新のリリースとし、動いているversionより新しければ知らせる。APIは使わない。読めない（networkが無い、timeout、形が違う）ときはqueueのeventに記録するだけで、askもattentionも出さず、次の回に読み直す。更新の無い日は何も出さない。
3. **既定は聞く。設定はhostに置く。** hostの設定（`host.toml`）で、聞く（既定）・聞かずに入れ替える（opt-in）・調べない、を選ぶ。バイナリはhostの全queueで共有され、repositoryの方針ではないので、commitされる`dagq.toml`には置かない。`up`のflagにもしない（付け忘れの`up`で黙って切れるため）。
4. **聞くときはinbox宛ての専用のaskにする。** optionsは`install`（その版に入れ替える）と`skip`（その版を飛ばし、次のリリースでまた聞く）。問いにはversion、今のversion、リリースの案内、答えで何が起きるか（互換のmigrationを適用して引き継ぐ、非互換なら改めて聞く）を書く。新しいリリースが出れば開いたaskを閉じて新しいaskにする。答えはこの仕組みを有効にしたliveなsupervisorが適用する。聞かない設定ではaskを開かずに5に進み、結果だけを知らせる。
5. **入れ替えは、queueのdirの下に`cargo install`してから`install`の手順に渡す。** `cargo install --locked dagq@<version>`を`--root`でqueueのdirの下の専用の置き場に入れ、そのバイナリを元にADR-0073決定12の確認、互換のmigration（決定5）、renameの差し替えと`.previous`（決定11）、liveなsupervisorの引き継ぎ（決定10）、見張りと戻し（決定13）を、source buildの自動更新と同じjobで行う。置き換える先はsupervisorが動いているバイナリのpathで、`~/.cargo/bin`に直接`cargo install`はしない（確認の前に置き換わり、`.previous`が残らず、利用者がバイナリを別の場所に置いていれば外れる）。cargoが無い・buildが失敗する・確認や見張りが失敗するときは、置き換えないか戻して、ADR-0073と同じ`update_failed`のaskで知らせる。
6. **人の入口として`dagq install`に「リリースから」の元を足す**（ADR-0073決定14をamends）。`install`はcheckoutとbuild済みのバイナリに加えて、crates.ioのリリース（versionの指定が無ければ最新）を元に取り、5と同じ置き場と手順で入れ替える。supervisorが居ないときや、askを待たずに上げたいときの入口で、4の答えもこれと同じ手順を動かす。決定14のそれ以外（確認・差し替え・引き継ぎ・drain・`--rollback`）は変えない。
7. **schemaの非互換のmigrationは、聞かない設定でも自動では適用しない。** buildしたリリースの`migrate --check`に非互換のmigrationがあれば、何も置き換えず、ADR-0073決定17の`approve_update`のaskで、drainが要ることとbuild済みのバイナリを示す。drainは人の了承で`install --allow-breaking`が行う。互換のmigrationは、4の`install`の答え（聞かない設定ではその設定）を同意とみなし、入れ替えの中で適用する。

pluginの更新との同期は[ADR-t618-2](2026-09-27-t618-2-plugin-follows-the-release-update.md)が決める。設定の欄名と値、askとeventのkind、置き場のpath、間隔の値は[Release update](../design/supervisor-lifecycle/release-update.md)が持つ。

## Alternatives

- **crates.ioのAPIか`cargo search` / `cargo info`**: APIは利用の方針（User-Agentと頻度の上限）があり、`cargo search`はそのAPIを使い、`cargo info`の出力は人向けで形が保証されない。
- **`~/.cargo/bin`に直接`cargo install`して`up`で引き継がせる**: cargoのmetadataは正しく保てるが、確認の前に置き換わり、戻し先が無い。人が手で行う経路としては今も使える。
- **検知の前にbuildしておき、非互換かどうかまで調べてから1回だけ聞く**: 人の決定（知らせてから、答えでcargo installする）に反し、答えを待たずにhostのCPUを数分使う。
- **`dagq.toml`か`up`のflagでopt-in**: 上の3の理由で退けた。
- **GitHub Releaseのassetを落として入れ替える**: Releaseのassetは今は`v0.2.0`だけで、導入の基本は`cargo install`（ADR-t614-1）なので、同じ経路で上げる。

## Consequences

- 外部のprojectは、askに`install`と答えるだけで、走っているrunを止めずにsupervisorが新しいリリースに入れ替わる。非互換のmigrationだけはdrainの了承が要る。
- 置き換えは`cargo install`の外で行うので、`~/.cargo/bin/dagq`を置き換えた後も`cargo install --list`は前のversionを示す。次に人が手で`cargo install dagq`を打っても同じversionを入れ直すだけで害は無い。`~/.cargo/bin`に`dagq.previous`が残る。
- hostの複数のqueueのsupervisorはそれぞれ調べて聞く。1つのqueueの答えでファイルは新しくなり、他のqueueの答えはbuildを省いて引き継ぎだけを行う（designに書く）。
- hostに`cargo`（とcrateの`rust-version`を満たすtoolchain）が要る。無いhostではaskの答えが`update_failed`になり、手で入れ替える方法を案内する。
- 実装は別のtaskで行う。
