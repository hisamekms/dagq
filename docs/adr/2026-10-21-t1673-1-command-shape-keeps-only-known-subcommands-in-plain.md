---
id: adr-t1673-1
type: adr
title: コマンドの形は決まった一覧にあるプログラム名とプログラムごとの決まった一覧にあるサブコマンドだけを平文で持ち、一覧に無いプログラム名・位置引数・値はdigestにする（ADR-t1662-3決定15をamends）
status: accepted
created: 2026-10-21
updated: 2026-10-21
accepted_on: 2026-10-21
amends:
  - adr-t1662-3 decision 15
owners:
  - hisamekms
tags:
  - runtime
  - security
  - observability
related:
  - adr-t1662-3
  - adr-t2268-1
  - design-measurement-commands
  - design-toolchain-profiles
---

# ADR-t1673-1: コマンドの形は一覧にあるプログラム名とサブコマンドだけを平文で持つ（ADR-t1662-3決定15をamends）

## Context

[ADR-t1662-3](2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)決定15は、サブコマンドの位置に来た語を形で決めて平文で持つとし、その位置に来た短い小文字の語（filter）が平文で残ることを受け入れ、残したくないrepositoryは`command_detail`の`program`か`redacted`を選ぶとした。

形だけで決めると、サブコマンドを持たないプログラムの位置引数も平文になる。`printf %s hunter2 > password.txt`の`hunter2`、`redis-cli auth hunter2`の`hunter2`のような秘密の値が、executorの送るstepに入りSessionStepStoreに保存される。既定の`shape`で秘密が残るのは決定14（値とfilterの平文は持たない）の趣旨に反する。人の判断（task 1673のrunのreviewの差し戻し、ask 1043）で、平文に残すのを既知のサブコマンドに限る。

サブコマンドを絞っても、プログラム名の位置は形で決まる。shellを字句に分ける作りでは句法の穴（`[[ "$value" == ok || hunter2 == "$value" ]]`の`||`の後の比較値など）のたびに、値がプログラム名の位置に来て平文で残る。人の判断（ask 1055）で、秘密を伏せる作りを許可の一覧の形に改め、字句の解析が誤っても一覧に無い語は平文にならないようにする。

digestは決まった作り方なので、平文で残らない語も、stepを読む者が候補の語のdigestを作って比べれば戻せる。`hunter2`のような低いentropyの値は辞書で推測できる。人の判断（ask 1093）で、telemetryに平文を残さない判断（ask 1043・1055）に沿い、digestをqueueごとの秘密の鍵によるkeyed digestにする。

一覧にあるプログラムでも、flagの名前は形で平文になる。`timeout 60 mytool --x`のように一覧にあるプログラムが走らせる一覧に無いプログラムのflag、`grep -e --pattern`のように`-`で始まるoptionの値、`git config --get-regexp 'x.*' --pattern`のように最初の位置引数で読むのを止めるparserの`-`で始まる位置引数、字句の解析の誤り（入れ子の引用、置換の中のcomment）でflagや代入の名前の位置に来た引用の中の語が、平文で残る。

## Decision

1. **プログラム名は決まった一覧にあるものだけ平文。** 部分の最初のプログラム名（pathを除いた名前）は、一覧が名指すときだけ平文で持ち、flagの名前とサブコマンドもそのときだけ平文で持つ。一覧に無いプログラム名はdigestで、その後の語はflagも含めて全てdigestにする。字句の解析が誤って値がプログラム名の位置に来ても、一覧に無ければ平文にならない。表示や比較の値を引数に取るプログラム（`echo`・`printf`・`test`・`[`・`[[`）は一覧に入れない。
2. **サブコマンドはプログラムごとの決まった一覧の語だけ平文。** サブコマンドの位置（決定15のまま）の語は、決定15の形に合い、かつそのプログラムの既知のサブコマンドの一覧にあるときだけ平文で持つ。一覧に無いプログラムは平文の語を持たない。一覧の形と位置の数え方は`domain::command_shape`の定義のそばのdoc commentが持つ。
3. **一覧の置き場は[ADR-t2268-1](2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)決定2・3に従う。** 道具に依らないruntimeの共通の道具（git・dagqとshellのよく使う道具）の一覧はshapeを作る副作用の無い関数のそばに置き、どのrepositoryでも効く。言語・道具ごとの一覧（cargo・rustupなど）はその道具のtoolchainのprofileが持ち、`dagq.toml`がprofileを宣言したときだけ効く。宣言の無いrepositoryでは共通の一覧のほかのプログラム名とサブコマンドは平文にならない。
4. **ほかの語はdigest。** 一覧に無い語は、位置の外の語と同じくdigestで持つ。digestは同じ語に同じ値になるので、値とfilterの区別（`--test it`と`--test plugin`、決定17）は平文を持たずに保てる。
5. **一覧にあるプログラムが走らせるコマンドの語はdigest。** 別のコマンド・scriptを引数に取るプログラム（`timeout`・`xargs`・`env`・shellのscript・`git bisect run`・`cargo run`・`rustup run`）は、走らせるものの最初の語から後をflagも含めて全てdigestにする。どこから走らせるかは一覧がプログラムごとに持つ。一覧に無いプログラムのflagの名前が一覧にあるプログラムのflagとして平文で残らないようにするため。
6. **字句を読み切れないコマンドは全ての語をdigest。** 引用・置換・subshell・`case`・`[[`が終わりで閉じていないか、開いていない`)`があるコマンドは、どの部分も一覧にあるプログラムを含めて全ての語をdigestにする。字句の解析が誤ってflagの名前や代入の名前の位置に値が来ても平文にならないようにするため。
7. **値や位置引数になりうる語は、flagの形でもdigest。** 一覧にあるプログラムでも、optionの値か位置引数になりうる語は`-`で始まってもdigestにする（`grep -e --pattern`、`git config --get-regexp 'x.*' --pattern`）。どの語がそうかは一覧の持つ知識（値を取らないoption、optionを位置引数の後にも1語ずつ読むparserか）で決め、一覧が知らないところは値とみなす。一覧の抜けがflagの名前を失うだけになり、値を平文に残さないため。`+`で始まる語は、toolchainをそれで選ぶと一覧が名指すプログラム（`cargo +nightly`）の最初の語だけを`+`の印とし、ほかは位置引数とみなす（`grep +pattern`）。走らせる先の境界（決定5）を探すときも同じく保守的に読む。
8. **分類はdigestの語でも続ける。** 決定16の規則のliteralは、平文の語だけでなくdigestの語とも照合する。一覧に無いサブコマンドとプログラム名（npmのscript名、makeのtarget）も分類から外れないようにするため。照合の手順は分類の規則の定義のそばのdoc commentが持つ。
9. 決定15のほか（一覧にあるプログラムのflag・`--`・リダイレクト・代入の名前）と決定14・16〜19は変えない。
10. **digestはqueueごとの秘密の鍵によるkeyed digest。** 決定15のdigestを、queueごとの秘密の鍵によるHMAC-SHA256（版の付いた語）にする。同じqueueの中では同じ語が同じdigestになるので、決定4・8の区別と照合は続けられる。鍵を知らなければ、低いentropyの値も辞書で照合できない。
    - **置き場**: 鍵はqueueのdirの中のserviceの私的なdirに、ファイルで置く。queueのDB・event・step・log・agentの環境と引数には出さない。
    - **作り方**: 鍵は乱数で、最初に要ったexecutorが作る。同時に作ろうとしたprocessがあっても、鍵は1つに決まる。
    - **鍵が無いとき**: 鍵を読めず作れないexecutorは、stepにコマンドの形を持たせない。鍵の無いdigestは作らない。
    - **回転**: 鍵は自動で回さない。回すときは、人がexecutorを止めて（`down`）ファイルを消し、次のexecutorが新しい鍵を作る。前後の鍵のdigestは照合できないので、回す前のstepはdigestの語では分類されず、保持の期間で消える。
    - 台帳を作る側が規則のliteralと照合するときも、同じqueueの鍵でdigestを作る（決定8）。
    - 鍵の大きさとdigestの桁、ファイルの名前と権限は、定義のそばのdoc commentが持つ。

## Alternatives

- **形だけで決める（決定15のまま）**: 位置引数の秘密が既定の`shape`で残る。repositoryに`program`か`redacted`を選ばせるだけでは既定が安全にならない。
- **位置引数を全て伏せ字（定数）にする**: `--test`の値やfilterの区別が消え、決定17の分類（`full_test`と`test`の区別）が作れない。digestなら照合できる。
- **一覧をrepositoryの設定にする**: 設定の無いrepositoryで既定が安全にならない。足すときは後の変更で、既定の一覧に重ねる形にする。
- **字句の解析を直して秘密らしい語を伏せる（ask 1055の前の作り）**: 句法の穴（`printf`の位置引数、`[[ … ]]`の比較値）のたびに平文が漏れ、穴を全て塞いだと示せない。
- **値を取るoptionの一覧を持つ**: 一覧に無い値を取るoptionの`-`で始まる値が平文で残る。値を取らないoptionの一覧なら、一覧の抜けはflagの名前を失うだけになる。
- **位置引数の後の`-`で始まる語を常にflagとして読む**: 最初の位置引数で読むのを止めるparser（`git config`）や値を複数の語で取るoption（`git update-index --cacheinfo`）の値が平文で残る。どのparserがそうかを全て数え上げられないので、知らないものは止めるものとみなす。
- **全ての道具の一覧をshapeの関数に持つ**: 言語・道具ごとの知識をprofileの外に置くことになり、ADR-t2268-1決定2に反する。
- **鍵の無いdigestのまま（決定15のまま）**: stepを読む者が辞書で低いentropyの値を戻せる。
- **鍵をqueueのDBに置く**: stepと同じDBを読む者が鍵も読める。
- **鍵の無いexecutorが鍵の無いdigestで送る**: その分のstepは辞書で戻せる。形を持たせないほうが平文を残さない。

## Consequences

- 平文に残るのは一覧にあるプログラム名とその既知のサブコマンドとflagの名前、shellの予約語、代入の名前だけになり、filterや値は位置にかかわらずdigestになる。digestは鍵を持つ側の照合のためで、鍵を持たない者には辞書で戻せない（決定10）。
- 別のqueueのstepとはdigestで照合できない。鍵を回した前後も同じ。
- 一覧に無い新しいサブコマンドはdigestで残り、一覧を足すまで平文で読めない。規則の分類は決定4・8でdigestと照合するので変わらない。
- `sh scripts/x.sh cargo test --locked`のようにscriptを走らせるコマンドは、scriptの後のflagの名前も平文で持たない。分類はdigestの語と照合する（決定8）。
- 値を取らないoptionの一覧に無いoptionのすぐ後のflag（`cargo build -j8 --release`の`--release`）は名前を平文で持たない。一覧を足すと平文で読める。
- optionを位置引数の後にも1語ずつ読むと一覧が名指さないプログラム（gitとshellの道具）は、最初の位置引数かoptionの値の後のflagの名前を平文で持たない（`git commit -m msg --amend`の`--amend`）。
- 字句を読み切れないコマンドは形の多くを失うが、そのような`-c`のscriptや入力はshellでも構文の誤りか、この字句の解析が誤る形で、どちらも平文を残さない側に倒す。
- profileの無い道具（npm・goなど）と一覧に無いプログラム（`echo`・`printf`を含む）は、プログラム名もflagの名前も平文で持たない。
- 今の姿は`domain::command_shape`のmodule docと[計測のコマンドの形と分類](../design/measurement-commands.md)が持つ。
