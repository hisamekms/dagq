---
id: plan-container-runtime-spike
type: plan
title: スパイク：container の runtime の前提の確認の手順（Claude の長期 token の代理・run ごとの clone の費用・dagq 専用の machine の最小の資源・sccache の cache の同時の書き込み）と、記録の表と結果が決める ADR の決定
status: active
created: 2026-10-11
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - security
related:
  - adr-t2113-1
  - adr-t2113-2
  - adr-t2113-3
  - adr-t2114-2
  - adr-t2114-4
  - adr-t2257-1
  - plan-headless-worker-spike
---

# スパイク：container の runtime の前提の確認の手順

goal 38（worker・resume と integrate の verification を run ごとの podman の container で動かす）の段 (1) の手順書。
4 つの項目は、ADR-t2113-1・ADR-t2113-2・ADR-t2114-2・ADR-t2114-4 が「実装の段で確かめる」とした前提で、結果がそれぞれの ADR の決定のどちらの形をとるかを決める。

- 流すのは人か inbox で、worker は流さない（worker は実 token・host の podman・本番と使い捨ての queue を使えない。AGENTS.md の「本番 queue と開発環境の境界」）。
  (a) は長期 token を扱うので人が流す（ADR-t2114-2 決定3: AI actor に token の発行も読み取りもさせない）。
  (b)〜(d) は人か inbox が流してよい。
- 結果は項目ごとに goal 38 の note に残す（下の「結果を goal 38 の note に残す書式」）。
  この文書の記録の表に写して `status` を `completed` にするのと、結果を要る実装の task に渡すのは、goal 38 の結果の取り込みの task が行う。
- 前提を満たさなかった項目は、人が決めて ADR に残す（項目ごとの「満たさないときに人が決めること」と、末尾の一覧）。

## 共通の前提

- 補助の script は [container-runtime-spike/](container-runtime-spike/) にある。
  python の script は標準ライブラリだけで、`python3 -I` で流す。
  sh の script は使い方を冒頭の comment に書いてある。
- 変数は sh で次のように置く（fish の session では `sh` を開いてから打つ）。
  `REPO` はこの repository の main checkout の path、`SPIKE` は Spike の作業の dir（`$HOME` の外の `$TMPDIR` の下）。

  ```sh
  REPO=<main checkout の path>
  S=$REPO/docs/plans/container-runtime-spike
  SPIKE=$(mktemp -d "$TMPDIR/dagq-spike.XXXXXX")
  ```

- 守ること:
  - token の値を表示しない・ファイルに書かない・コマンドの引数と環境変数に入れない。
    長期 token が画面に出るのは人が打つ `claude setup-token` の発行の表示だけで、人はそれを転送の echo しない入力に貼り、値は転送の process のメモリにだけある。
    Claude Code に渡す `ANTHROPIC_AUTH_TOKEN` は run token に当たる意味のない値（`spike-run-token`）で、長期 token ではない。
  - 人の既定の podman machine と既定の接続を変えない（ADR-t2113-1 決定4）。
    Spike の machine は `dagq-spike` の名前で作り、podman のコマンドは `--connection dagq-spike` を付けて打つ（script がそうする）。
    ほかの machine が動いていれば `machine.sh` は起動を拒むので、止めるかは人が決める（Spike は止めない）。
    Podman は接続が無いときに init で作る最初の接続を既定にし（`--update-connection` に依らない）、machine の rm で既定の接続が消えると残りから選び直す。
    そこで `machine.sh` は、人の既定の接続が在ること（Spike の接続でないこと）を `up`・`set`・`down` の前に確かめ、無ければ何もせずに止まる（既定を決めるのは人）。
    `podman machine init` と `podman machine start` には `--update-connection=false` を付け、変更を尋ねさせない（この flag を持たない Podman では動かない。[podman-machine-start](https://docs.podman.io/en/latest/markdown/podman-machine-start.1.html#update-connection-u)）。
    init・start・rm の後に既定が動いていたら、確かめておいた人の既定に戻す。
  - 本番 queue と使い捨ての queue を動かさない。
    queue に触るのは結果の note だけで、固定バイナリ `~/.local/bin/dagq` で `$REPO` の中で打つ（AGENTS.md の開始時の制約）。
  - host の sccache の server を起動しない（ADR-t2086-1）。
    (d) の sccache は container の中だけで動かす。
- 環境として記録するもの: 日付（JST）、`sw_vers -productVersion`、`uname -m`、`podman --version`、`claude --version`（(a)）、`git --version`（(b)）、machine の CPU とメモリ（(c)・(d)）、測った時刻の host の load（`uptime`）。
- 後片付け（全項目の後）: `sh "$S/machine.sh" down` で Spike の machine を消し、`rm -rf "$SPIKE"` で作業の dir を消す。
  (a) の長期 token の扱いは (a) の後片付け。

## 結果を goal 38 の note に残す書式

項目ごとに 1 つの note にし、固定バイナリで残す。
(a) の手順は作業の dir に `cd` するので、note は `$REPO` に入ってから打つ。

```sh
cd "$REPO"
~/.local/bin/dagq note --goal 38 --kind spike --text "$(cat "$SPIKE/note-a.md")"
```

note の本文の書式:

```text
container-runtime-spike (a) Claude の長期 token の代理
日付: 2026-10-DD（JST）
環境: macOS <版> <arch>・<項目で使った道具と版>・machine dagq-spike <CPU>/<メモリ>（(c)・(d)）・load <uptime の値>
表:
<この文書のその項目の記録の表と同じ列で、測った行をすべて>
合否: 合 / 否 / 一部（否の行とその理由）
人の決定: <否のときだけ。決めたことと、残した（残す）ADR の ID>
手順: docs/plans/container-runtime-spike.md
```

## 記録の区分と作り直せる範囲

- 正本（SSOT）: 項目ごとの goal 38 の note（queue の event）。
  Spike の結果として残るのはこれだけなので、作業の dir を消す前に、記録の表の行・合否・判断に使った材料の要点（(a) の `error_body` と `stream-signs.py` の行、(c) の落ちた test と失敗の文、(d) の server の error の行）を note に写す。
- ビュー: この文書の記録の表。
  goal 38 の結果の取り込みの task が note から写すもので、note から何度でも作り直せる。
- 材料: 作業の dir（`$SPIKE`）の下の CSV（`b-*.csv`・`c.csv`・`d.csv`）・転送の log（`a-*.log`）・Claude Code の出力（`a-*.jsonl`）・container の build と test の log・メモリの標本・sccache の stats と server の log・clone。
  (c)・(d) の build の出力と sccache の cache は machine の中の named volume にあり、build の出力は script の終わりに、cache は `machine.sh down` で消える。
  commit せず、後片付けで消え、消えた後は note に写したものしか残らない。
- 道具: [container-runtime-spike/](container-runtime-spike/) の script は commit してあり、git から何度でも取り出せる。
- 測り直せる範囲: (b)〜(d) は同じ手順でいつでも流し直せるが、host の load・repository の大きさ・道具の版で値は変わるので、前の値と同じにはならない（比べるときは note の環境の行を合わせる）。
  (a) は人が token を発行し直せば流し直せるが、規約の判断と API の応答（認証切れの body・利用上限）はその時点のもので、同じになるとは限らない。

## (a) Claude の長期 token を代理で付けて送ること

### 目的

container に資格情報を置かず、queue の broker が run token を `claude setup-token` の長期 token に差し替えて `api.anthropic.com` に送る形（ADR-t2114-2 決定1・2）が、規約と動作の上で成り立つかを確かめる。

### 結果が決める ADR の決定

ADR-t2114-2 決定5（前提として確かめる 3 つ）と決定6（だめなら API key に替える）。
全部が合なら決定1〜4 の形で実装し、どれかが否で broker の側の手当て（応答の素通し・header の扱い）で直せなければ、決定6 の条件に当たる。

### 前提と後片付け

- 流すのは人。
  Claude Code の実体（`~/.local/bin/claude`。cmux の shim でないもの）を使う。
- 人の Claude Code のログイン（keychain）を使わず refresh も起こさないよう、Claude Code は空の `CLAUDE_CONFIG_DIR` で起動する。
  要求が転送を通ったこと（転送の log に行が出ること）で、人のログインで送られていないことを確かめる。
- 転送は `token-proxy.py` で、host の `127.0.0.1` だけで待ち受け、入ってきた要求の `Authorization` と `x-api-key` を捨てて長期 token を付け、応答を chunk ごとにそのまま返す。
  長期 token は起動のときに terminal で echo せずに受け取り（terminal でなければ起動を拒む）、process のメモリにだけ置く。
  ファイル・引数・環境変数からは読まない。
  `--invalid` は token でない決まった値を使い、入力を求めない。
  log には要求ごとの method・path・status・大きさ・時間・chunk の最大の間隔・`message_stop` の数・利用上限の header・error の body の先頭を書き、token の値は書かない（書きそうな箇所は置き換える）。
  止めるとき（Ctrl-C）に `--scan` の dir の下のファイルに token が入っていないかを調べ、件数と path だけを出す。
- 後片付け: `rm -rf "$SPIKE/a-config"` で Claude Code の設定の dir を消す。
  長期 token はどのファイルにも残っていないので、実装で使うかは人が決め、使わないなら人が失効させる（失効の方法は Anthropic の案内に従う）。

### 手順

1. **規約（人の判断）**: 人が Anthropic の規約（利用規約・Usage Policy・Claude Code の legal and compliance の案内）を読み、サブスクリプションの長期 token を host の転送（のちの broker）が代理で付けて Claude Code の要求を送ることが許されるかを判断する。
   AI は判断しない。
   読んだ文書の URL・読んだ日・判断（許される / 許されない / 判断できない）と根拠の箇所を記録する。
   許されないなら手順 2 以降は流さず、「満たさないときに人が決めること」へ進む。
2. **token の発行と転送の起動**: 人が `claude setup-token` で長期 token を発行する。
   別の terminal で転送を起動し、出た入力の求めに表示された token を貼る（echo されず、ファイルに保存しない）。

   ```sh
   python3 -I "$S/token-proxy.py" --log "$SPIKE/a-proxy.log" --scan "$SPIKE" --port 8787
   ```

3. **通常の応答**: Claude Code の非対話の 1 turn を転送に通す。

   ```sh
   mkdir -p "$SPIKE/a-config" "$SPIKE/a-cwd" && cd "$SPIKE/a-cwd"
   CLAUDE_CONFIG_DIR="$SPIKE/a-config" ANTHROPIC_BASE_URL=http://127.0.0.1:8787 ANTHROPIC_AUTH_TOKEN=spike-run-token \
     ~/.local/bin/claude -p --output-format stream-json --verbose '1 行で挨拶してください' > "$SPIKE/a-normal.jsonl"
   python3 -I "$S/stream-signs.py" "$SPIKE/a-normal.jsonl"
   tail -n 5 "$SPIKE/a-proxy.log"
   ```

   401 で body が OAuth の token を受けない旨（beta の header が要るなど）を言うときは、その行を記録し、body が名指す値で転送を `--add-beta <値>` を付けて起動し直して（token をもう一度貼る）同じ turn を流す。
   header を足して通ったら、その手当てが broker に要ることを記録する（決定6 の「broker の側の手当て」で直せる場合に当たる）。
4. **長い SSE の stream**: 応答が 1 分を超える turn を同じ形で流す。

   ```sh
   CLAUDE_CONFIG_DIR="$SPIKE/a-config" ANTHROPIC_BASE_URL=http://127.0.0.1:8787 ANTHROPIC_AUTH_TOKEN=spike-run-token \
     ~/.local/bin/claude -p --output-format stream-json --verbose \
     '1 から 3000 までの整数を 1 行に 1 つずつ、省略せずにすべて出力してください' > "$SPIKE/a-long.jsonl"
   python3 -I "$S/stream-signs.py" "$SPIKE/a-long.jsonl"
   ```

   転送の log の `/v1/messages` の行で最も長い `secs` が 60 秒に届かなければ、数を増やして流し直す。
5. **無効な token の認証切れ**: 転送を Ctrl-C で止め（このとき手順 7 の確認が出る）、`--invalid` で別の log に起動し直して同じ turn を流す。
   Claude Code は 401 を数分やり直すので、3 分で打ち切る。

   ```sh
   python3 -I "$S/token-proxy.py" --invalid --log "$SPIKE/a-invalid.log" --port 8787
   ```

   ```sh
   CLAUDE_CONFIG_DIR="$SPIKE/a-config" ANTHROPIC_BASE_URL=http://127.0.0.1:8787 ANTHROPIC_AUTH_TOKEN=spike-run-token \
     perl -e 'alarm shift; exec @ARGV' 180 ~/.local/bin/claude -p --output-format stream-json --verbose '1 行で挨拶してください' > "$SPIKE/a-invalid.jsonl"
   python3 -I "$S/stream-signs.py" "$SPIKE/a-invalid.jsonl"
   tail -n 3 "$SPIKE/a-invalid.log"
   ```

   転送の log の `status` と `error_body`、`stream-signs.py` の出した行を記録する。
   期限切れの token は作れないので確かめず、確かめなかったと記録する。
6. **利用上限**: 手順 3〜5 の転送の log の `ratelimit`（`anthropic-ratelimit-` で始まる応答の header と `retry-after`）と、`stream-signs.py` の `rate_limit_event` の行を記録する。
   Spike の間に利用上限に当たったら（人が上限の近い時期に流してもよい）、その応答の `status`・`error_body`・`stream-signs.py` の行を記録する。
   当たらなければ、上限の応答は出せなかったと記録する。
7. **token がファイルに無いことの確認**: 手順 5 で長期 token の転送を止めたときに出る `files under ... holding the token: <件数>` を記録する（値は出ない）。
   手順 5 の後に作ったファイルは長期 token を知らない転送の出力なので、確認に含めなくてよい。
   無効な token の転送も最後に Ctrl-C で止める。

### 今の分類のコードと照らす

手順 5・6 で `stream-signs.py` が出した行と転送の log の `status`・`error_body` を、Spike の日の main のコードの判定に当てはめ、認証切れ（`TurnFailure::Authentication`）か利用上限（`TurnFailure::UsageLimit`）に分類されるかを確かめる。
判定の条件はこの文書に写さず、コードを読む。

- 入口: `src/infrastructure/claude_turns.rs` の `ClaudeTurnReader`（stream-json の turn を読む）と、`src/infrastructure/claude.rs` の `job_wall`（出力の文を読む）。
- 分類の値: `src/domain/turn.rs` の `TurnFailure`。
- 記録の表の「今のコードの分類」には、当たった関数と分類の値か、どの判定にも当たらなかったことを書く。

### 合否の基準

- 規約: 人が「許される」と判断した。
- 通常の応答: 転送の log の `/v1/messages` の行が `status` 200 で `had_authorization` が true、`stream-signs.py` の `result` が `is_error` false。
- 長い SSE: `secs` が 60 秒以上の `/v1/messages` の行が少なくとも 1 つあり、その行が `upstream_complete` true・`message_stop` 1 以上・`client_closed` と `upstream_broke` が無く、`result` が `is_error` false で出力が 3000 まで途切れていない。
- 認証切れ: 転送の log の `status` が 401（か 403）で、「今の分類のコードと照らす」で認証切れに分類される。
- 利用上限: 上限の応答が出たときは、「今の分類のコードと照らす」で利用上限に分類される。
  出せなかったときは合否を「未確認」とし、`ratelimit` の header と `rate_limit_event` が転送を通って出ていること（素通しされていること）を記録する。
- token の確認: 手順 7 の件数が 0。

### 記録の表

| 確認 | 日付 | Claude Code の版 | status | 最長の secs | max_gap_secs | upstream_complete / message_stop | 読めた印（stream-signs の行） | 今のコードの分類 | 手当て（--add-beta など） | 合否 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 規約 | 2026-10-11 | - | - | - | - | - | - | - | - | 一部（人の判断: 個人利用なら適合しそう、broker による代理は明示的に保証されない。note 151554） |
| 通常の応答 | 2026-10-11 | 2.1.296 | 200（had_authorization true） | 1.857 | 0.666 | true / 1 | result success・is_error false | - | なし | 合（note 152330） |
| 長い SSE | 2026-10-11 | 2.1.296 | 200（had_authorization true） | 70.883（3000 は 53.744 で 60 秒に届かず、4000 で流し直した） | 0.524 | true / 1 | result success・is_error false、出力は 1〜4000 が欠けずに揃う | - | なし | 合（note 152330） |
| 無効な token | 2026-10-11 | 2.1.296 | 401（error_body: `authentication_error`「OAuth access token is invalid.」） | 0.273 | - | - | assistant の error `authentication_failed`・result is_error true・api_error_status 401（401 は 1 回で、やり直しは起きなかった） | `ClaudeTurnReader` が `TurnFailure::Authentication`（api_error_status 401 でも同じ） | なし | 合（note 152330） |
| 利用上限 | 2026-10-11 | 2.1.296 | 上限に当たらず | - | - | - | 応答の header `anthropic-ratelimit-unified-*`（5h・7d の使用率・status allowed・overage rejected）が転送を素通しした。rate_limit_event の行は出なかった | 未確認（上限の応答を出せなかった） | - | 未確認（note 152330） |
| token がファイルに無い | 2026-10-11 | - | - | - | - | - | 転送を止めたとき `files under ... holding the token: 0` | - | - | 合（note 152330） |

環境は macOS 14.7.4 arm64・空の `CLAUDE_CONFIG_DIR`・転送は `--add-beta` なし（note 152330）。
値は 1 回の実測の読み取りで、手順 2〜7 は人が流し、log と出力の読み取りは inbox が行った。

結果の当てはめ:

- 通常の応答・長い SSE・認証切れの分類は通り、broker の側の手当て（応答の素通しの直し・`--add-beta` などの header）は要らなかった。
  broker の直しが要るのは、素通しの SSE か分類が通らないときで、今回は当たらない。
- 規約は「一部」で、長期 token が規約か動作の上で通らない場合の影響は、人が `host.toml` で API key を選ぶことに留まる（長期 token と API key の両方に対応して人が選ぶ方針。note 151554）。
- 利用上限は未確認で、人が次のように決めた（note 152336）: 利用上限の応答は未確認のまま実装（task 2177 など ADR-t2114-2 決定1〜4）に進め、本番で初めて利用上限に当たったときに runtime が `TurnFailure::UsageLimit`（控え）に分類するかを確かめ、合わなければ直す。
  この (a) の結果と人の決定は [ADR-t2177-1](../adr/2026-10-12-t2177-1-broker-swaps-the-run-token-for-a-claude-credential-chosen-in-host-toml.md) 決定6 が出典として引き、長期 token と API key を `host.toml` で人が選ぶ形は同じ ADR の決定1〜3 が持つ。

### 満たさないときに人が決めること

ADR-t2114-2 決定6 の条件（規約で許されない、または通常の応答・SSE・分類のどれかが broker の側の手当てで直せない）に当たったら、人が API key に替えるかを決める。
替えると費用がサブスクリプションから従量に移るので、費用の受け入れも人が決める。
決めたことは ADR-t2114-2 の amends か置き換えの ADR に残す（AI は決めない）。
利用上限が「未確認」のときは、未確認のまま実装に進むか、上限の応答を確かめるまで決定1〜4 の実装の着地を待つかを人が決める（ADR-t2114-2 の Consequences: 前提の確認が終わるまで実装を着地させない）。

## (b) run ごとの clone の費用

### 目的

container の run に渡す独立した clone（ADR-t2113-2 決定1）を作る時間と disk、同時に作るときの合計、run branch を bundle で取り込む時間を測り、推奨の形の費用を受け入れるかを決める材料にする。

### 結果が決める ADR の決定

ADR-t2113-2 決定4（推奨の形の費用が実測で受け入れられないときだけ次善の形を採る）と「費用の見積もり」の節の実測。
どちらの clone の作り方（path と `--no-hardlinks`、`file://` の URL）を実装の既定にするかの材料にもなる。

### 前提と後片付け

- 人か inbox が流す。
  podman も token も要らない。
- `clone-cost.sh` は `REPO` を読むだけで、clone と bundle と取り込みの先はすべて作業の dir の下に作り、終わりに消す（残し方は script の冒頭の comment）。
  bundle の取り込みは作業の dir の下の別の clone（制御側に当たる）に行い、`REPO` には fetch しない。
- run branch は、clone の HEAD の上に新しい文の 1 commit を置いたもので、1 run の変更（多くは数百 KB 以下）に当たる。
  commit の大きさ（KiB）は手順のコマンドの第 3 引数で渡し、ここでは 256 と 2048 で測る。
- worker の build が重い時間を避け、`uptime` の load を記録する。

### 手順

```sh
git -C "$REPO" count-objects -vH
for i in 1 2 3; do
  sh "$S/clone-cost.sh" "$REPO" "$SPIKE/b-$i" 256 | tee "$SPIKE/b-256-$i.csv"
done
sh "$S/clone-cost.sh" "$REPO" "$SPIKE/b-big" 2048 | tee "$SPIKE/b-2048.csv"
```

3 回の中央値を表に写す。
`count-objects` の loose と pack の大きさも記録する（path の clone は loose の object をそのまま複写するので、host の gc の具合で費用が変わる）。

### 合否の基準

受け入れの目安（超えたら人が決める）:

- clone: 同時 4 本でも全体の時間が 60 秒以下で、1 本の disk が 1 GiB 以下（run の `target/` の GB 単位より小さい）。
- bundle: 作成と取り込みの合計が 10 秒以下。

目安を満たす作り方が 1 つ以上あれば合とし、時間と disk が小さい方を推奨として記録する。

### 記録の表

| 作り方 | 同時の本数 | 全体の時間（秒、中央値） | 1 本の disk（KiB） | うち .git（KiB） | 合否 |
| --- | --- | --- | --- | --- | --- |
| path・--no-hardlinks | 1 | 9.345 | 296092 | 260544 | 合 |
| path・--no-hardlinks | 2 | 11.136 | 296092 | 260544 | 合 |
| path・--no-hardlinks | 4 | 13.464 | 296092 | 260544 | 合 |
| file:// の URL | 1 | 1.587 | 約 55100 | 約 19560 | 合 |
| file:// の URL | 2 | 2.004 | 約 55100 | 約 19560 | 合 |
| file:// の URL | 4 | 2.811 | 55108 | 19560 | 合 |

| run branch の変更 | bundle の大きさ（byte） | 作成（秒） | 取り込み（秒） | 合否 |
| --- | --- | --- | --- | --- |
| 256 KiB | 199162（中央値） | 0.063 | 0.092 | 合 |
| 2048 KiB | 1589153 | 0.134 | 0.102 | 合 |

出典は note 151589（2026-10-11、inbox が流した）。
環境は macOS 14.7.4 arm64・git 2.39.3・load 5.12〜8.02（worker 3 本が動いていた）、`REPO` の `count-objects` は loose 26334 個 219.73 MiB・in-pack 47100・packs 23・size-pack 34.14 MiB・garbage 0。
値は 1 回の Spike の読み取りで、256 KiB は手順の 3 周の中央値、2048 KiB は 1 周。
各周は script のとおり作り方ごとに同時 1・2・4 本を順に流し、作り方を交互にしていない。

合否は合（両方の作り方が目安を満たす）。
推奨は `file://` の URL で、時間と disk が path・`--no-hardlinks` の約 1/5（.git は約 1/13）。
path の clone は `REPO` の loose の object をそのまま複写するので、host の gc の具合で費用が大きく変わる。
前提を満たしたので、(b) の ADR は書かない（ADR-t2113-2 決定4 の次善の形は採らない）。

### 満たさないときに人が決めること

どちらの作り方も目安を超えたら、費用を受け入れて推奨の形のままにするか、次善の形（共通の dir の mount と config・hooks の読み取り専用の重ね）を採るかを人が決める。
次善を採るなら、他の run の branch と main の ref を書き換えうることを既知の制限として残し、ADR-t2113-3 決定1 の amends も要る（ADR-t2113-2 決定4）。
目安そのものを変えるのも人が決める。

## (c) dagq 専用の machine の最小の資源

### 目的

worker・resume と integrate の verification の build と test が通る、dagq 専用の machine の最小の CPU とメモリを測る（resource broker の値の CPU 1・メモリ 1 GiB では通らない見込み）。
測るのは dagq の Rust の build で、結果はこの repository の dogfooding の host の `host.toml` の推奨値と worker の cache の形の判断の材料にし、runtime の既定や言語の前提にはしない（ADR-t2257-1 決定6）。

### 結果が決める ADR の決定

ADR-t2257-1 決定6（ADR-t2113-1 決定6 を amends）。測った通る最小の値は、この repository を動かす host の `host.toml` の推奨値として docs に書き、runtime の既定にしない。

### 前提と後片付け

- 人か inbox が流す。
  podman（host にあるもの）を使い、Spike の machine `dagq-spike` を作る。
  machine は `$HOME` を mount せず、作業の dir（`$SPIKE/m`）だけを mount する（`machine.sh up` が mount を確かめ、ほかの mount があれば止まる）。
- container は rust-toolchain.toml の channel の `rust:<channel>` の image（unit test が使う git を含む）で、toolchain（rust-toolchain.toml の component と target）と crate は作業の dir の下に置く。
  `CARGO_HOME` を作業の dir にすると image の rustup の proxy（`/usr/local/cargo/bin`）が `CARGO_HOME` の `bin` に rustup を探して見つけられないので、`prepare` は rustup を動かす前に image の `/usr/local/cargo/bin` を作業の dir の `cargo-home` に複写する（既にあれば複写しない）。
- build の出力（`CARGO_TARGET_DIR`）は作業の dir（virtiofs の mount）に置かず、machine の中の podman の named volume `dagq-spike-target-<label>` に置く（host の mount の上では build script の link が「cannot find ... .rcgu.o」で落ちる）。
  `run` は組ごとに volume を消して作り直してから build し（cache なしの build）、終わりに消す。
  log・メモリの標本・CSV は作業の dir に書き、host から読む。
  準備（`prepare`）だけが network を使い、build と test は `--network none`・読み取り専用の root filesystem・capability の削除・特権の昇格の禁止・pids の上限で流す。
  `/tmp` は test が script を置いて実行できるよう exec の tmpfs にする。
  非 root の uid と container の memory・cpu の上限は付けない（この項目は machine の大きさを測り、container の上限は実装が決める）。
- `rustup toolchain install` は引数なしで rust-toolchain.toml の toolchain を入れる（rustup 1.28 以降）。
  image の rustup がそれより古ければ、`prepare` の失敗を記録し、channel を名指して入れ直す。
- 後片付け: (d) も流すなら machine を残し、流さないなら `sh "$S/machine.sh" down`。
  `down` は machine を消す前に Spike の named volume（`dagq-spike-*`）を消し、volume は machine の disk とともにも消えるので残らない。

### 手順

大きい組から流し、通った組を基準に小さくする。

```sh
mkdir -p "$SPIKE/m"
sh "$S/machine.sh" up "$SPIKE/m" 4 8192
sh "$S/container-build.sh" prepare "$SPIKE/m" "$REPO" "$(sccache --version | cut -d ' ' -f 2)"
sh "$S/container-build.sh" run "$SPIKE/m" c4m8 4 | tee -a "$SPIKE/c.csv"
sh "$S/machine.sh" set 4 6144
sh "$S/container-build.sh" run "$SPIKE/m" c4m6 4 | tee -a "$SPIKE/c.csv"
sh "$S/machine.sh" set 2 4096
sh "$S/container-build.sh" run "$SPIKE/m" c2m4 2 | tee -a "$SPIKE/c.csv"
```

build は `--all-targets`、unit test は `--lib --bins` で、組ごとに build の出力の named volume を作り直してから流す（cache なしの build）。
test が落ちたら `$SPIKE/m/logs/<label>-test.log` から落ちた test の名前と失敗の文を記録する。
落ちた test を合否から外さない（Linux の CI は失敗を通さず、macOS に固有の test は cfg で分ける。`docs/design/linux-ci.md`）。
どの組でも同じく落ちることは資源以外の原因の手がかりにはなるが、それだけで OS に固有とも準備の不足とも決めない。

### 合否の基準

- 組ごとに、build と test の `exit` が 0、`oom_killed` が false、`kernel_oom` が 0 なら通る。
- `oom_killed` が true か `kernel_oom` が 1 以上の組は否（資源が足りない）。
- OOM が無く build か test が落ちた組は「未確認」とし、落ちた test と失敗の文を記録する。
  原因（今の main の不具合・Linux で落ちる test・image や toolchain の準備の不足・資源）の判断はこの項目の合否と分け、goal 38 の note の「人の決定」の欄か、別の調べとして人か planner に渡す。
  原因が直って同じ組で通るまで、その組を最小の候補にしない。
- 通る組のうち最小を選ぶ。
  通る組が無く「未確認」の組があるときは、最小の資源を決めずに「未確認」として記録する。
  その組の build の時間が最大の組の 1.5 倍を超えるなら、時間と資源のどちらを取るかを人に上げる。
- どの組も通らなければ、より大きい組（例 6/10 GiB）を足して流し、host（8 コア / 16GB）の中で通る組が無ければ否。
- 同時に動く container の数（worker と verification）の分の大きさは、記録した `peak_mem` から実装が見積もる。

### 記録の表

| 組（CPU / メモリ） | 段 | 時間（秒） | exit | oom_killed | kernel_oom | peak_mem | 落ちた test と失敗の文 | 合否（合 / 否 / 未確認） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 / 8 GiB | build | 16.3 | 101 | false | 0 | 900.1M | build script の link が「ld: cannot find .../build_script_build-*.rcgu.o」 | 未確認 |
| 4 / 8 GiB | test | - | 101 | - | - | - | build が落ちたので意味が無い | 未確認 |
| 4 / 6 GiB | build | 17.0 | 101 | false | 0 | 1.18G | 同じ link の失敗 | 未確認 |
| 4 / 6 GiB | test | - | 101 | - | - | - | build が落ちたので意味が無い | 未確認 |
| 2 / 4 GiB | build | 23.8 | 101 | false | 0 | 557.2M | 同じ link の失敗 | 未確認 |
| 2 / 4 GiB | test | - | 101 | - | - | - | build が落ちたので意味が無い | 未確認 |

上の行は 2026-10-11 の 1 回目（note 152044。podman 6.1.2（applehv）・image `rust:1.98.1`・load 4〜19）で、最小の資源は未確認のまま決めていない。
資源ではなく、手順書の script の不具合 2 つで測れなかった。

- `container-build.sh` の `prepare` が `CARGO_HOME` を作業の dir にするので、image の rustup の proxy が rustup を見つけられずに落ちる（1 回目は写した script で `cargo/bin` を複写して流した）。
- build の出力（`CARGO_TARGET_DIR`）を machine に virtiofs で mount した host の dir に置くと、link が「cannot find ... .rcgu.o」で落ちる。
  4 / 8 GiB で build の出力を machine の中の podman の named volume に置くと、build は 343 秒で通った（test は流していない）。

2 つはこの文書の手順と script で直した（rustup の複写と、build の出力の named volume）。
測り直しは行わず、通る最小の資源は確かめていない。
この repository の host の `host.toml` には、1 回目で build が通った 4 / 8 GiB を暫定の推奨として [運用](../development/operations.md) の「containerのmachineの資源（`host.toml`）」に書いた（test まで通るかは container の runtime のスモークで確かめる）。

### 満たさないときに人が決めること

host の中で通る組が無いか、通る最小の組が host に残す資源（人の作業と host の run）を圧迫するときは、machine の大きさ・同時に container で動かす run の数・container にする適用先の範囲のどれを変えるかを人が決め、ADR-t2257-1 決定6 の amends に残す。

## (d) sccache の local の disk cache の同時の書き込み

### 目的

worker・resume の container ごとの sccache の server が、同じ worker の cache の dir を同時に使って壊れず、当たり、大きさの上限が効くかを確かめる。

### 結果が決める ADR の決定

ADR-t2114-4 決定5（同時の書き込みと大きさの上限に耐えないときは、run ごとに種から複写する形か、1 つの server に寄せる形に替える）。

### 前提と後片付け

- 人か inbox が流す。
  (c) の machine と `prepare` の後に流す（`prepare` が container で使う sccache を、(c) の手順のコマンドで渡した host と同じ版で、local の disk cache だけの feature で作業の dir に入れる）。
- machine は 4 CPU / 8 GiB 以上にし、container ごとの `CARGO_BUILD_JOBS` は 2 本なら 2、3・4 本なら 1 にする。
- 各 container は `--network none` で、自分の loopback に自分の server を build の前に起動し（ADR-t2114-4 決定1・2）、`SCCACHE_DIR` に machine の中の 1 つの共有の named volume `dagq-spike-sccache` を mount する（worker の cache を machine の中の queue ごとの named volume に置く形に揃える）。
  cache は round を跨いで残し、`cold` の round だけが volume を作り直して空にする。
- build の出力は container ごとの named volume `dagq-spike-target-sccache-<i>` に置き、round ごとに作り直し（cache なしの build）、終わりに消す（(c) と同じく host の mount の上では link が落ちる）。
- cache の大きさは machine の中で volume を `du` で読み、CSV の `cache_kib` に出す。
  server の error の log・stats・build の log は作業の dir の下のファイルに書き、host から読む。
- 後片付け: `sh "$S/machine.sh" down`（Spike の named volume も消える）と `rm -rf "$SPIKE"`。

### 手順

```sh
sh "$S/machine.sh" set 4 8192
sh "$S/sccache-concurrency.sh" "$SPIKE/m" 2 cold 10G 2 | tee -a "$SPIKE/d.csv"
sh "$S/sccache-concurrency.sh" "$SPIKE/m" 2 warm 10G 2 | tee -a "$SPIKE/d.csv"
sh "$S/sccache-concurrency.sh" "$SPIKE/m" 3 cold 10G 1 | tee -a "$SPIKE/d.csv"
sh "$S/sccache-concurrency.sh" "$SPIKE/m" 3 warm 10G 1 | tee -a "$SPIKE/d.csv"
sh "$S/sccache-concurrency.sh" "$SPIKE/m" 4 cold 10G 1 | tee -a "$SPIKE/d.csv"
sh "$S/sccache-concurrency.sh" "$SPIKE/m" 4 warm 10G 1 | tee -a "$SPIKE/d.csv"
podman machine ssh dagq-spike "du -sk '$(podman --connection dagq-spike volume inspect --format '{{.Mountpoint}}' dagq-spike-sccache)'"
```

続けて、上限を warm の後の cache の大きさの半分ほど（例 `300M`）にして 2 回流し、追い出しと当たりを見る。

```sh
sh "$S/sccache-concurrency.sh" "$SPIKE/m" 4 limit1 300M 1 | tee -a "$SPIKE/d.csv"
sh "$S/sccache-concurrency.sh" "$SPIKE/m" 4 limit2 300M 1 | tee -a "$SPIKE/d.csv"
grep -i -l 'error' "$SPIKE"/m/logs/sccache-*.server.log
```

時間の列は container ごとの build の時間。
列の stats が空なら、`$SPIKE/m/logs/sccache-<n>-<round>-<i>.stats.json` を直接読む。

### 合否の基準

- どの round でも build の `exit` が 0。
- `cache_errors`・`read_errors`・`write_errors` が 0 で、server の error の log に cache の読み書きの error が無い。
- warm の round で、container ごとの当たり率（hits / (hits + misses)）が 0.9 以上。
- limit の round で、終わった後の cache の volume の大きさ（`cache_kib`）が上限の 1.2 倍以下で、build が通る。
  上限を大きく超え続けるときは、server ごとに大きさを数えて互いの書き込みを数えていない現れとして記録する。

### 記録の表

| 本数 | round | container | 時間（秒） | exit | requests | hits | misses | cache_errors | read_errors | write_errors | cache の大きさ（KiB） | 当たり率 | 合否 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 2〜4 | cold・warm・limit1・limit2 | 全部 | - | 26 の build のうち 25 が 101（4 本の warm の 2 本目だけ 0） | - | - | - | 0 | 0 | 0 | 54204 → 116132（limit の 300M でも追い出しは起きず） | warm で 2 本は 1.0、3・4 本は 0.79〜1.0（build が途中で落ちたので材料にならない） | 未確認 |

上の行は 2026-10-11 の 1 回目（note 152045。machine 4 CPU / 8 GiB・sccache 0.18.0）で、server の log に error は無かった。
build が (c) と同じ link の失敗で落ちたので、同時の書き込みで壊れないことも、上限が効くことも確かめられていない。
この置き場はこの文書の手順と script で直した（cache と build の出力を machine の中の named volume に置く）。
測り直しはこの手順で人か inbox が流し、結果の取り込みと (d) の ADR（ADR-t2114-4 決定5）は task 2280 が持つ。

### 満たさないときに人が決めること

同時の書き込みで error か build の失敗が出るか、上限が効かないときは、worker の cache を run ごとに種から複写する形か、worker・resume の側の 1 つの server に寄せる形のどちらに替えるかを人が決め、ADR-t2114-4 の amends に残す（決定5）。
当たり率だけが低いときは、替えるか、当たり率を受け入れて今の形で進むかを人が決める。

## 前提を満たさなかったときの一覧

| 項目 | 否の条件 | 人が決めること | 残す先 |
| --- | --- | --- | --- |
| (a) | 規約で許されない、または通常の応答・SSE・分類が broker の側の手当てで直せない | API key に替えるか（費用がサブスクリプションから従量に移ることの受け入れを含む）。利用上限が未確認のまま進むか | ADR-t2114-2 の amends か置き換え（[ADR-t2177-1](../adr/2026-10-12-t2177-1-broker-swaps-the-run-token-for-a-claude-credential-chosen-in-host-toml.md) が amends し、利用上限を未確認のまま進める人の決定を持つ） |
| (b) | どちらの clone の作り方も目安を超える | 推奨の形のままにするか次善の形を採るか、目安を変えるか | ADR-t2113-2 の amends（次善を採るなら ADR-t2113-3 決定1 の amends も） |
| (c) | host の中で通る組が無い、または通る最小が host を圧迫する | machine の大きさ・同時の container の数・container にする適用先の範囲 | ADR-t2257-1 決定6 の amends |
| (d) | 同時の書き込みの error・build の失敗・上限が効かない | 種から複写する形か 1 つの server に寄せる形か | ADR-t2114-4 決定5 の amends |
