---
id: adr-t2397-1
type: adr
title: crateの分割に委ねた境界の規則（L1・L3・L6・L8・L9・C5・C8）は、domainのcrateを切り出さないgoal 121の結論を受けて、crateを1つのまま、srcを構文で読むRustのtest（moduleの参照の検査）で守り、レイヤー・contextごとのcrateには今は分けない（ADR-t2391-1決定1・4・5、ADR-t1545-1決定4をamends）
status: accepted
created: 2026-10-18
updated: 2026-10-18
accepted_on: 2026-10-18
amends:
  - adr-t2391-1 decision 1
  - adr-t2391-1 decision 4
  - adr-t2391-1 decision 5
  - adr-t1545-1 decision 4
owners:
  - hisamekms
tags:
  - architecture
  - testing
related:
  - adr-t2391-1
  - adr-t1545-1
  - adr-0013
  - adr-t2287-1
  - plan-domain-crate-measurement
  - design-architecture
---

# ADR-t2397-1: crateの分割に委ねた境界の規則は、crateを1つのまま、moduleの参照を構文で読むRustのtestで守る

## Context

[ADR-t2391-1](2026-10-12-t2391-1-boundary-rules-to-compiler-clippy-and-rust-tests.md)決定4は、L1・L3・L6・L8の参照されないこと・L9のcontextのmoduleどうし・C5・C8をcrateの分割で守るとし、切り方をgoal 121の結論の後のADRに委ねた。分けないなら別の移し先を同じADRが決める。
goal 121の結論（[domainのcrateの測定](../plans/domain-crate-measurement.md)の「結論」）は**今はdomainをcrateに切り出さない**。applicationを変えた後のbuildは速くなるが、domainを触る変更（`src`の変更の56.4%）で得が消え、重み付きの短縮は約9〜10秒で負荷の揺れより小さい。L1は既に違反0で、切り出しは違反0の検査をcompileへ移すだけである。domainを触るcommitの91.7%がcrateをまたぎ、一度きりの手間（pathの書き換え471か所・orphan ruleの26）も掛かる。contextごとのcrateは結論の範囲外で、application・infrastructureの線は測っていない。goal 195の制約は測り直しを禁じる。

base（`ea7629f1`）で数えた、crateに分けるなら解く循環と違反:

| 線 | 数えたもの | 値 | 数えたコマンド |
| --- | --- | --- | --- |
| レイヤー | レイヤーの間の参照の循環 | 0。domain→`build_id`だけ、application→domain・`build_id`・`migration_numbers`、infrastructure→application・domain・`migration_numbers`で、内向きだけ | `sh scripts/check-layer-deps.sh`→ok、`grep -cvE '^#\|^$' .config/layer-deps-allow.txt`→0、下の(a) |
| context | `src/application/ports/`のmoduleの循環 | `shared`と`execution`・`host`・`observation`の3組の2周と、それらを通る3周（`host`→`execution`→`shared`→`host`、`observation`→`execution`→`shared`→`observation`ほか）。`shared`の`Queue`・`StateStore`がsupertraitとして全contextのportを名指すため | 下の(b) |
| context | portのmoduleからportでないapplicationのmoduleへの参照 | `planning`→`GraphInput`・`TaskPage`・`TaskQuery`、`shared`→`timestamp`・`unix_seconds`（`src/application/mod.rs`に定義）、`execution`→`review_programs::SnapshotProgram` | (b)の`crate::application`と`super::super`の行 |
| context | 全contextのportを1つにした型を取るapplicationのコード | `dyn Queue`・`dyn StateStore`が19 file・51か所 | `grep -rhoE 'dyn (Queue\|StateStore)\b' src/application \| wc -l`、`grep -rlE 'dyn (Queue\|StateStore)\b' src/application \| wc -l` |
| context | crateの境界でcompileが落とすようになる今の違反 | C5が12（うち行き先のtaskのあるもの3）。C1の20はcrateでは落ちない | `grep -cE '^C5 ' .config/context-boundaries-allow.txt`、`grep -cE '^C1 ' .config/context-boundaries-allow.txt` |

```sh
# (a) top-level module ごとの crate:: の参照（コメントの行を除く）
for m in domain application infrastructure compose view runtime lifecycle; do
  grep -rhE 'crate::' src/$m src/$m.rs 2>/dev/null | grep -vE '^\s*//' \
    | grep -oE 'crate::(domain|application|infrastructure|compose|view|runtime|lifecycle|build_id|migration_numbers)\b' | sort | uniq -c
done
# (b) ports の module どうしの参照
for f in planning execution observation host shared; do
  grep -nE '^\s*(pub )?use |super::' src/application/ports/$f.rs | grep -vE '^\S+:\s*//'
done
```

## Decision

### 1. crateは1つのまま、委ねた規則はmoduleの参照のRustのtestで守る

workspaceにせず、レイヤーごと・contextごとのcrateに今は分けない。
ADR-t2391-1決定4がcrateに委ねた規則は、srcを構文で読むRustのtest（moduleの参照の検査）に移す。

- testは`src/`の全てのRustのfileをdev-dependencyのparserで読み、`use`の木（波括弧・`self`・`super`・別名・glob）と式と型の中のpathを、crateの根からのmoduleのpathに解く。再公開（`pub use`）はその定義のmoduleまで辿り、traitは名前でなく定義のpathで識別する。コメント・文字列・doc commentは数えない。
- moduleのpathからレイヤーとcontextを決め、pathから導けない所有とportの公開先は、ADR-t2391-1決定3のC1・C7・X1のtestと同じtest側の型付きの表を読む。parserと表はそのtestと共有し、規則ごとに別の仕組みを作らない。
- 例外（`build_id`・`migration_numbers`、C8の例外の組）と今ある違反は同じ表に理由と行き先付きで置く。表に無い参照と、参照が消えた項目で落ちる（ADR-t2391-1決定3の扱いのまま）。
- `cargo test`のunitの範囲に置き、着地の検証で毎回流れる。shellのscriptとLLMのagentには残さない。

### 2. 規則ごとの境界

| 規則 | 守るもの |
| --- | --- |
| L1 | 決定1のtest（`#[cfg(test)]`の中も数える） |
| L2 | clippy（ADR-t2391-1決定2のまま）。domainのcrateの依存で表す部分は、crateを切り出さないので無い |
| L3 | 決定1のtest |
| L4 | clippy（ADR-t2391-1決定2のまま） |
| L6 | 決定1のtest |
| L8 | 参照されないことは決定1のtest（L1・L3・L6として）、新しいmoduleを外に足さないことは全体のreview |
| L9 | `src/compose/`の外からの参照は可視性（既に成り立つ）、contextのmoduleどうしと`use super::*`は決定1のtest |
| C3 | 可視性（状態の欄をcontextのmoduleに絞る、ADR-t2391-1決定1のまま）、残りは全体のreview |
| C5 | portの参照は決定1のtest（traitをpathで識別）、一部公開のportのmethodの単位の参照とcontextの決まらないmoduleは全体のreview |
| C8 | 決定1のtest、例外の中身は全体のreview |

全体のreviewはADR-t2391-1決定1の「全体のreview」と同じ置き場である。

### 3. 先に直す違反と、解かない循環

- 決定1のtestは今の違反を表に持てるので、移す前に直す違反は無い。C5の12件は理由と行き先（または未登録）ごと表へ移し、行き先のtaskが着地とともに外す。
- Contextの循環（portの`shared`の集約と、portでないmoduleへの参照）は、crateに分けないので解かない。分ける判断をし直すときの前提として上の数とコマンドを残す。

### 4. 切り出す順と退役

1. レイヤーの参照の段: 決定1のtestを足し、L1・L3・L6・L8とL9のcontextのmoduleどうしを移し、`scripts/check-layer-deps.sh`からそれらの規則を外す。parserと表はADR-t2391-1決定3のtestのものを使うので、その後に置く。
2. contextの参照の段: C5・C8を同じtestに移し、`.config/context-boundaries-allow.txt`のC5の項目を表へ移し、2本のscriptからC5・C8を外す。
3. 退役: ADR-t2391-1の全ての移し先と上の2段の着地の後に、2本のscriptと`.config/`の3つの一覧、CIのstep、`scripts/check-scripts-root.sh`の項目、`dagq.toml`の`[review.programs.check-context-boundaries]`、`docs/development/`のverifyの案内を消す（ADR-t2391-1決定5の3）。

### 5. amendsの範囲

- **ADR-t2391-1決定1**: L1・L3・L6・L8の参照されないこと・L9のcontextのmoduleどうし・C5・C8の移し先を、crateの分割から決定1のtestに変える。L2の「分割の後はcrateの依存にも現れる」は無くなる。他の行は保つ。
- **ADR-t2391-1決定4**: 「crateの分割で守る」と「それまでscriptが見る」を、決定1のtestで守り、その着地までscriptが見る、に変える。測定が足りなければ測るtaskを先に足すことは、分ける判断をし直すときに保つ。
- **ADR-t2391-1決定5**: 1の「型とcrateの分割が着地するまで」のcrateの分割と、3の「crateの分割（決定4）が着地して」を、決定4の2段の着地に変える。退役の順（agentを先、scriptは規則ごとに削り最後に消す）は保つ。
- **ADR-t1545-1決定4**: 「レイヤーを分けるためだけのcrateの分割はしない（ADR-0013のAlternativesのまま）」を保ち、理由をgoal 121の測定に、contextごとのcrateにも広げ、依存の向きを見るものをreviewと棚卸しから決定1のtestにする。
- **[ADR-0013](0013-layered-architecture-and-type-function-style.md)のAlternatives**の「crateを分割する」の項の、依存方向の違反をreviewと棚卸しの受け入れ条件で見る理由を、上と同じく決定1のtestで見る形に読み替える。番号付きの決定ではないので`amends`と`amended_by`には書かず（ADR-t2391-1の「amendsしないADR」と同じ扱い）、ADR-0013は`related`に置く。決定1（crateの分割は必須としない）は変えない。

## Alternatives

- **domain・application・infrastructureのレイヤーごとのcrate**: レイヤーの循環は0で線は引けるが、domainのcrateはgoal 121が退け、L1・L3・L6は既に違反0で得はcompileへ移すだけである。application・infrastructureの線のbuildと変更の費用は測っておらず、このgoalでは測り直せない。
- **contextごとのcrate**: C5・C8・L9は落とせるが、portの`shared`の集約と根への参照の循環、`dyn Queue`の51か所、C5の12件を先に解く必要があり、1段1 worktreeの大きさを大きく超える。
- **moduleをcontextの下にまとめ直し、可視性（`pub(in path)`）で守る**: applicationのportとinfrastructureの実装が共通の祖先を持つように木を組み替えるが、ADR-t1545-1のレイヤーを先に分ける木を崩し、ほぼ全てのfileが動く。
- **dylintで参照の向きを見る**: nightlyとcargo-dylintがhostとtoolchainに要る（ADR-t2391-1決定3、goal 195の制約）。

## Consequences

- crateに分ける判断をし直すのは、goal 121の見直す条件（seedの本番入り、`src/domain`を触らない変更の割合が70%以上、L1・L2の違反の再発）か、portの`shared`の集約が解けてC5の表が空になったときで、そのときの新しいADRがtestの規則のうちcrateの依存で表せるものを外す。
- moduleの参照の規則の例外と違反が、C1・C7・X1と同じtest側の1つの表に集まる。
- architecture.mdの各規則の「検査」の行は、決定4の各段の着地ごとに直す。
