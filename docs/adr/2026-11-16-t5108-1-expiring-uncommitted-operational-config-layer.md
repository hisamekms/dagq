---
id: adr-t5108-1
type: adr
title: main checkoutにcommitしない期限つきの運用設定を重ねる
status: accepted
created: 2026-11-16
updated: 2026-11-16
accepted_on: 2026-11-16
owners:
  - hisamekms
tags:
  - architecture
  - configuration
related:
  - adr-t4727-1
  - adr-t2257-1
---

# ADR-t5108-1: main checkoutに期限つきの運用設定を重ねる

## Context

運用の一時の設定をmain checkoutの`dagq.toml`の未commitの変更に置くと、同じファイルを変えるrunのff-onlyの着地とぶつかる。
設定の短い試行にも開発のレーンを通すと、ベータ開発の速度を落とす。
reviewを通らない値を許す範囲と、戻し忘れを防ぐ期限と、人が気づける記録が要る。

[ADR-t4727-1](2026-11-11-t4727-1-disk-needs-may-be-relaxed-for-a-while-by-a-person-or-the-inbox.md)の決定は全部保つ。
必要量を緩めるCLI・権限・最低ラインの保護・queueのeventを正本とする境界は変えない。
同ADRのAlternativesが退けた`dagq.toml`への一時の値と違い、ここで採るのはcommitしない別ファイルで、期限を必須とし、eventとstatusに見せる運用設定の層である。
既存のADRをamendsも置き換えもしない、新しい設定の入口とする。

## Decision

1. **main checkoutの作業ファイルだけに重ねる。** `dagq.toml`の隣の`dagq.local.toml`だけを、同じ書式で読む。main checkoutの設定を読むすべての読み手は、許すtableのkeyごとにbaseの値を上書きし、無いkeyをbaseから引き継ぐ。runのworktree・着地するbranchのtree・reviewのsubagentの設定・review用に生成する設定など、commitした内容を読む経路には重ねない。main checkoutを読まないdeploymentのremote rulesにも重ねない。
2. **期限を必須にする。** RFC 3339で期限を持ち、期限が無い・読んだ時点から定数の上限を超える先・期限切れなら層を使わない。上限の値は定数のdoc commentに置く。
3. **運用のknobだけをallowlistで許す。** 稼働中に値を差し替えられ、各passか各回に読み直すtableだけを許す。最終の一覧と読み手・読む時点は定義のそばのdoc commentが持つ。着地の先とpush、検証と関門・e2e・review・承認条件、認可と監査、workerとreviewへの入力やworkerの振る舞い、toolchain・image・runtime、計画の語彙は拒む。着地の前の関門を変えないため、`[recheck]`は許可するtableに含めない。許さないkeyが1つでもあれば層全体を使わない。
4. **commitされないことを検査する。** Gitが`dagq.local.toml`を追跡していれば層を使わない。追跡の有無を確かめられないときも使わない。
5. **誤った層はbaseで動かす。** 構文や値の誤りも層全体を使わず、`dagq.toml`だけで動く。base自体の誤りの扱いは変えない。
6. **書くのは人とinboxだけ。** hostのファイルをworkerは書かない。containerのworkerはmain checkoutに届かず、hostのworkerはoperationsの規則で禁じる。ファイルの書き込みはroleで検査できないため、守りは変化のeventによる検出とする。
7. **監査と見え方をqueueと人の入口に置く。** 層の変化・期限切れ・拒否・削除をqueueに残し、その記録から効いている層を特定できること。人とinboxがstatusとdoctorで効いているkeyとcommitした値との違いを見られること。記録と表示の実装は後続に置き、eventのkindと欄、statusとdoctorの項の名前と形は定義のそばのdoc commentで決める。
8. **projectによらない仕組みにする。** ファイル名・期限の上限・allowlistは共通で、外部のprojectもそのまま使える。
9. **次のpassから効かせる。** 稼働中のsupervisorは再起動なしに読み直し、期限切れ・拒否・削除でも次のpassからbaseに戻る。起動時だけ読んでいたtableを許すなら各passへ読み直しを配線する。起動のflagは層より優先する。読めないbaseは使っている値を保ち、同じerrorのwarnを繰り返さない。

決定を変える必要が実装で分かったら、人に聞いて止める。

## Alternatives

- **(b) queueのDBへCLIで上書きを書く**: 設定は同期のファイル読みで多くの文脈から読まれる。client modeにも届けるには大きな配線が要り、人が求めたベータの速度に合わない。
- **(c) host.tomlにkeyを足す**: hostの値だけを持つ境界を崩し、projectごとの値も置けない。
- **denylistにする**: 新しいtableが既定で上書きできてしまい、reviewを通らない層の権限が意図せず広がる。

## Consequences

commitした設定の作業ファイルを汚さずに一時の値を試せ、期限が自動で元へ戻す。
追跡の検査とallowlistが安全に関わる設定の境界を保つ一方、hostで誰が書いたかはファイルだけでは証明できない。
監査と表示が揃うまでは、値の差し替えの中核だけでは運用の見え方の約束は完結しない。
