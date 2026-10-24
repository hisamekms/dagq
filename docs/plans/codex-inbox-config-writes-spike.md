---
id: plan-codex-inbox-config-writes-spike
type: plan
title: スパイク：Codex の対話の TUI の起動が、欄の無い config.toml に書き足すものと、それを起動の引数で止められるか
status: completed
created: 2026-10-25
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - provider
  - inbox
related:
  - adr-t1181-1
  - plan-codex-inbox-spike
  - plan-codex-inbox-hook-trust-spike
---

# スパイク：Codex の対話の TUI の起動が config.toml に書き足すものと、起動の引数で止められるか

goal 77 の Spike（task 3338）。
[ADR-t1181-1](../adr/2026-10-24-t1181-1-codex-inbox-provider-selection-gaps-unavailability-and-records.md) の Context と「実装の順」(b) は、TUI の起動が人の `~/.codex/config.toml` を書くかを実装の前に確かめるとした。
[codex-inbox-spike](codex-inbox-spike.md) の「task 1174 の結果と `~/.codex/config.toml`」は、空の `CODEX_HOME` で `[tui] screen_reader_detection_done = true` が書かれ、`tui.model_availability_nux` は人の設定に欄があったので書かれなかっただけとみた。
この task は測って書くだけで、`src/` と `docs/design/` は変えていない。

問い: codex-cli の対話の TUI を Codex の inbox の起動の形（`--no-daemon`、project の trust の `-c`、`-c check_for_update_on_startup=false`）で起動したとき、欄の無い `config.toml` に Codex が書き足すものは何か。
そして、起動の引数（`-c tui.screen_reader_detection_done=true`・`-c tui.model_availability_nux={…}`・`-c tui.show_tooltips=false`）を足すと書き足しが止まるか。

## 環境と対象

- 対象の commit: `dcf869b0a6edc8a0da3071f6c97facdc81a38805`（この run の base）。
  dagq の runtime は使っていない
- 日付: 2026-10-25 04:44〜05:00（JST。起動は 04:45:03〜04:47:28）。
  host は macOS 14.7.4（arm64、Darwin 23.6.0）
- codex-cli **0.160.0**（`~/.local/bin/codex` → `~/.codex/packages/standalone/current/bin/codex`、`releases/0.160.0-aarch64-apple-darwin`）。
  この session の PATH の先頭の `codex` は cmux の shim で 0.164.0 を返すが、dagq の runtime が使う実体と同じ `~/.local/bin/codex` で測った。
  0.164.0 は測っていない
- route: 対話の TUI を `--no-daemon` で起動した（人の Codex の app-server の daemon に繋がない）。
  `CODEX_HOME` は回ごとに新しく作った scratch の dir で、未 login（`auth.json` を置いていない）。
  全部の回が login の画面（`Welcome to Codex …`・`Sign in with ChatGPT …`）で止まり、Ctrl-C で終えた
- scratch: `$TMPDIR/dagq-worker-t3338-2d2tV9/repo`（`git init -b main`、`seed.txt` の 1 commit）。
  queue も supervisor も使っていない
- pty: tmux 3.6a を専用の socket（`tmux -L dagq-t3338 -f /dev/null`）の detached の session で動かし、起動から 10 秒後に `capture-pane -p` で画面を読んだ
- source: openai/codex の tag `rust-v0.160.0`（commit `a956835d020762cb2b570053af06f643a11c0ecc`）を scratch に sparse で clone して読んだ（読むだけ。build していない）

### 使わなかったもの

- `~/.codex/auth.json` は写さず、scratch の `CODEX_HOME` に link もしていない。
  login はしていない
- `--dangerously-bypass-hook-trust`、sandbox・承認を外す引数は使っていない
- 人の `~/.codex`・`~/.claude` は書き換えていない。
  本番の queue に触れていない

## 打った起動の引数

[`bin/run.sh`](codex-inbox-config-writes-spike/bin/run.sh) `<回> <E|M> [-c の引数 …]` が、回ごとに次をする。

1. 新しい `CODEX_HOME`（`<scratch>/home-<回>`）を作る。
   E は空の dir、M は project の trust だけを書いた `config.toml` を置く（`[projects."<scratch>/repo"]` の `trust_level = "trusted"`）
2. 起動の前の `config.toml`（あれば）と更新時刻、人の `~/.codex/config.toml` の更新時刻と sha256（[`bin/snap.sh`](codex-inbox-config-writes-spike/bin/snap.sh)）を残す
3. scratch の repository で次を起動する

   ```sh
   env CODEX_HOME=<scratch>/home-<回> ~/.local/bin/codex --no-daemon \
     -c 'projects={"<scratch>/repo"={trust_level="trusted"}}' \
     -c 'check_for_update_on_startup=false' <足した -c の引数>
   ```

4. 10 秒後に画面を読み、Ctrl-C を 2 回送って終える
5. 起動の後の `config.toml` と更新時刻、人の設定の更新時刻と sha256 を残し、前後の `config.toml` の diff を取る

「全部」の回に足した引数は次の 3 つ。
`tui.model_availability_nux` の値は、人の設定にある欄と同じ形と回数にした。

```text
-c tui.screen_reader_detection_done=true
-c tui.model_availability_nux={gpt-6-astra=4,"gpt-6.1-sol"=4}
-c tui.show_tooltips=false
```

どの回も設定の誤りで落ちず、login の画面まで進んだ（`-c` の値は受け入れられた）。

## 結果表

測定の記録は [codex-inbox-config-writes-spike/](codex-inbox-config-writes-spike/) に置いた。
この dir は 1 回きりの測定の出力で、設計の文書ではない。
`bin/` は測定の script、`runs/run<回>/` は回ごとの足した引数（`extra-args.txt`）・起動の時刻（`start.txt`）・前後の `config.toml`・diff（`config.diff`）・更新時刻（`mtime.txt`、epoch 秒）・画面（`screen.txt`）、`snap.log` は人の設定の更新時刻と sha256 である。
script は記録を scratch の `out/run<回>/` と `out/snap.log` に書き、この dir にはそれを `runs/run<回>/` と `snap.log` に写した（`config.diff` の見出しの path は scratch の `out/` のまま）。
`bin/` の script と上の scratch の作り方があれば、同じ版の codex で作り直せる。

人の `~/.codex/config.toml` は全部の記録（測定の前 04:45:02 と各回の前後）で、更新時刻 1790953911（2026-10-03 00:11:51 JST）、sha256 `5d764192d9c005b1984f043a9c7471f147481224523c646ae789feb3e9829f0b` のまま変わらなかった。

| 回 | CODEX_HOME の初め | 足した -c の引数 | 起動後に書き足された欄（config.toml の diff） | config.toml の更新時刻の前後（JST） | login の画面で止まったか | 人の ~/.codex/config.toml の sha256 と更新時刻が前後で一致したか |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | E（空） | なし | `[tui]` `screen_reader_detection_done = true`（file を新しく作った） | 前: file 無し / 後: 04:45:03 | 止まった | 一致（前 04:45:03、後 04:45:16） |
| 2 | E（空） | 全部（3 つ） | `[tui]` `screen_reader_detection_done = true`（file を新しく作った） | 前: file 無し / 後: 04:45:20 | 止まった | 一致（前 04:45:20、後 04:45:33） |
| 3 | M（trust だけ） | なし | `[tui]` `screen_reader_detection_done = true`（trust の表の後に足した） | 前: 04:45:38 / 後: 04:45:39 | 止まった | 一致（前 04:45:38、後 04:45:52） |
| 4 | M（trust だけ） | 全部（3 つ） | `[tui]` `screen_reader_detection_done = true`（trust の表の後に足した） | 前: 04:45:52 / 後: 04:45:52（同じ秒。中身は変わった） | 止まった | 一致（前 04:45:52、後 04:46:05） |
| 5 | E（空） | `-c tui.screen_reader_detection_done=true` だけ | `[tui]` `screen_reader_detection_done = true`（file を新しく作った） | 前: file 無し / 後: 04:47:15 | 止まった | 一致（前 04:47:15、後 04:47:28） |

- 4 つの組（回 1〜4）の全部で `screen_reader_detection_done` の書き足しが残ったので、その欄の key だけを足す起動（回 5）を E で 1 回足した。
  `tui.model_availability_nux` と `tui.show_tooltips` は、どの回でも書かれなかった（止まらずに残った欄ではないので、1 つずつ足す起動はしていない）。
- 起動は 5 回（上限 12 回）、時間は約 14 分（上限 45 分）。
  打ち切った組は無い。
- `config.toml` のほかに、scratch の `CODEX_HOME` には起動のたびに `installation_id`・`*.sqlite`（`state_5`・`logs_2`・`goals_1`・`memories_1`・`queue_1`）・`skills/.system/`・`.tmp/plugins-clone-*` が作られた。
  これらは Codex の状態で人の設定ではないので、判定の対象にしていない（人の `~/.codex` には既にある）。

## 欄ごとの判定と根拠

### `tui.screen_reader_detection_done`: 書かれ、起動の引数では止まらない

- 測定: E・M の両方で、`-c tui.screen_reader_detection_done=true` を足しても足さなくても書かれた（回 1〜5）。
- source（`codex-rs/tui/src/screen_reader.rs` の `initialize_with_probe` と `initialize_file`）: 書くかどうかは `ConfigLayerStack::effective_user_config()`（`codex-rs/config/src/state.rs`）が返す User の層（`$CODEX_HOME/config.toml` と profile の file）だけを見て、そこに `tui.screen_reader_detection_done` があれば書かない。
  `-c` は SessionFlags の層なので、この判断に入らない。
  書く先は `get_user_config_file()` の file で、無ければ作る。
  OS の読み上げ（macOS は VoiceOver）が有効と判ったときは、`tui.animations` が無ければ `animations = false` も書く。
- 呼び出し（`codex-rs/tui/src/startup_orchestration.rs`）は `loader_overrides.ignore_user_config` が偽のときだけ動くが、0.160.0 の `codex --help` にこれを立てる flag は無い。
- 起動の引数のうち `-p, --profile <name>` だけは、人の `config.toml` への書き足しを避けうる（測っていない）。
  profile を付けると User の層の上に `$CODEX_HOME/<name>.config.toml` が足され、`effective_user_config()` は両方を合わせ、`get_user_config_file()` と `LocalSettings.user_config_path` はその profile の file を返す（`codex-rs/config/src/state.rs`・`codex-rs/tui/src/local_settings.rs`）。
  したがって、その file に `[tui] screen_reader_detection_done = true` を置いておけば書かれず、`tui.model_availability_nux` の回数もその file に書かれる。
  ただしその file は人の `CODEX_HOME`（`~/.codex`）の中に置くしかなく、goal 77 の制約（人の `CODEX_HOME` を書き換えず、設定は起動の引数か queue の dir に置く）に反するので、この Spike は止める手段に数えない。
  `CODEX_HOME` ごと queue の dir に移す手は、`auth.json` を写すか link することになり、秘密を写さない制約に反する。
- 書かれる条件: User の層の `config.toml` に `tui.screen_reader_detection_done` の欄が無いとき（値が false でも欄があれば書かない）。
  この host の人の `~/.codex/config.toml` には既に `screen_reader_detection_done = true` があるので、今の人の設定には書かれない。

### `tui.model_availability_nux`: 未 login で測れず unresolved。source では `-c tui.show_tooltips=false` で止まる

- 測定: どの回でも書かれなかった。
  未 login の起動は login の画面で止まり、model の一覧を受けとって最初の thread を始める段まで進まないので、書く処理に届かない。
  したがって測定からは止まるかを決められない。
- source（`codex-rs/tui/src/app/startup_prompts.rs` の `prepare_startup_tooltip_override`、`codex-rs/tui/src/app/startup.rs` の呼び出し）: 新しい thread で起動したとき、次の順に判じて書く。
  1. `is_first_run`（`codex-rs/tui/src/lib.rs`。cwd の project の trust_level が無い）か `config.tui.show_tooltips` が偽なら、何も選ばず書かない
  2. 受けとった model の一覧で最初の `availability_nux` を持つ model を選び、その model の `shown_count` が上限（`MODEL_AVAILABILITY_NUX_MAX_SHOW_COUNT` = 4）未満のときだけ案内を出す
  3. その model の回数を 1 足し、`ConfigEditsBuilder::set_model_availability_nux_count` で User の層の file の `[tui.model_availability_nux]` を消して、合成した回数の表で書き直す（`codex-rs/core/src/config/edit.rs` の `model_availability_nux_count_edits`）
- `show_tooltips` と `model_availability_nux` は合成した設定（`codex-rs/core/src/config/mod.rs` の `cfg.tui`）から読むので、`-c` が効く。
  - TUI は合成した `Config` から作った `LocalSettings`（`codex-rs/tui/src/local_settings.rs`）でこの 2 つを読む。
  - `-c tui.show_tooltips=false` は 1 で返し、どの model の案内でも書かない。
  - `-c tui.model_availability_nux={<model>=4,…}` は、名指した model の案内だけを 2 で止める。
    一覧の最初の案内が名指していない model（新しい model）なら、3 で `-c` の値を含む合成した表ごと人の file に書くので、止める手段として使えない。
- project の trust を `-c` で渡すと `is_first_run` は偽になる（inbox の起動の形）ので、1 の `is_first_run` には頼れない。

### `tui.show_tooltips`: 書かれない

- 測定: どの回でも書かれなかった。
- source: TUI は `show_tooltips` を読むだけで（`codex-rs/tui/src/local_settings.rs`・`app/turn_tips.rs`・`history_cell/session.rs`）、起動で書く処理は無い。

## 判定

**does_not_hold。**

根拠:

- `tui.screen_reader_detection_done` は、欄の無い `config.toml` に E・M の両方で書き足され、`-c tui.screen_reader_detection_done=true` を足しても（回 2・4・5）止まらなかった。
  source でも、書くかの判断は User の層だけを見て `-c` を見ないので、起動の引数で止める手段は 0.160.0 に無い（`-p` で人の `CODEX_HOME` に profile の file を置く手は、上のとおり制約に反するので数えない）。
  止まらない欄が残るので、問いの「全部止まる」は成り立たない。
- `tui.model_availability_nux` は未 login で書く処理まで進まず、測定では unresolved。
  source では、`-c tui.show_tooltips=false` が書く前の判断で返すので止まる見込みだが、実の起動では確かめていない。
- `tui.show_tooltips` は書かれない（測定と source）。
- 人の `~/.codex/config.toml` は、5 回の起動の前後で更新時刻と sha256 が一致した（`CODEX_HOME` を scratch にしたので、人の file はそもそも読み書きの対象でない）。
- 条件: codex-cli 0.160.0、`--no-daemon` の対話の TUI、未 login の scratch の `CODEX_HOME`、macOS（VoiceOver は無効）。
  Codex の版が上がれば書く欄と判断が変わりうる。

この判定が計画に意味すること: 人の `config.toml` に `tui.screen_reader_detection_done` の欄が無い host で `dagq inbox` が Codex の TUI を開くと、Codex 自身が人の file にこの欄を書き足し、dagq の起動の引数では防げない。
この host の人の設定には既に欄があるので、今は書かれない。
`tui.model_availability_nux` は `-c tui.show_tooltips=false` を足せば止まる見込みで、確かめるのは login した起動（人か inbox の手のスモーク）になる。

## 後続の計画への推奨

判定は does_not_hold なので、次の「does_not_hold のとき」を採る。

### does_not_hold のとき（今回）

- **task 3331**（Codex の起動の経路）:
  - Codex の inbox の起動に `-c tui.show_tooltips=false` を足し、unit test で起動の引数に入ることを確かめる（`tui.model_availability_nux` の書き足しを止める見込みの引数）。
    `-c tui.model_availability_nux={…}` は足さない（名指していない model の案内で、合成した表ごと人の file に書かれるため）。
    `-c tui.screen_reader_detection_done=true` は効かないので足さない。
    `-c tui.show_tooltips=false` は案内だけでなく、起動と turn の tips も全部止める（`codex-rs/tui/src/app/turn_tips.rs`・`codex-rs/tui/src/history_cell/session.rs`）が、inbox には要らない。
  - design（[up-down](../design/supervisor-lifecycle/up-down.md) の「`dagq inbox`」と [provider-lifecycle](../design/provider-lifecycle.md)）に、止まらない欄と書かれる条件を書く。
    人の `config.toml` に `tui.screen_reader_detection_done` の欄が無いと、Codex の TUI が最初の起動で `[tui] screen_reader_detection_done = true`（読み上げが有効なら `animations = false` も）を書き足し、起動の引数では止められない。
    ADR-t1181-1 の Consequences の「TUI の起動が人の `~/.codex/config.toml` を書くか」は、未 login の起動ではこの欄が書かれ、起動の引数で止まらないと確かめた、として扱う。
    `tui.model_availability_nux` は source から `-c tui.show_tooltips=false` で止まる見込みで、login した起動では確かめていない。
    login した後の model の移行の案内（`codex-rs/tui/src/app/startup_prompts.rs` の移行の処理）も、人が選んだときに人の file を書きうるが、この Spike は測っていない。
    人の `CODEX_HOME` に profile の file を置けば避けうるが（`-p`）、人の設定を書き換えない制約を変えることになるので、採るなら人の判断が要る。
  - 起動の前に dagq が人の file を読んで欄の有無を確かめ、無ければ人に知らせる（書くのは Codex で dagq は書かない）かは、3331 が design で決める。
- **task 3334**（skill と manual-smoke）:
  - manual-smoke の Codex の inbox のスモークに、`dagq inbox` の起動の前と後で人の `~/.codex/config.toml` の更新時刻と sha256（と中身の diff）を取る確かめを入れる。
  - login した Codex で、`-c tui.show_tooltips=false` を付けた起動が `[tui.model_availability_nux]` を書かないこと（この Spike で unresolved の部分）を、その確かめで見る。
    確かめる意味があるのは、一覧の最初の案内の model の回数が人の設定で 4 未満のとき（新しい model の案内が出るとき）で、そうでなければ付けなくても書かれない。
  - skill に、人の設定に `tui.screen_reader_detection_done` が無ければ Codex が最初の起動で書き足すことを書く。

### holds のとき（Codex の版が上がり、User の層の外から止められるようになったとき）

- **task 3331**: 止める引数を Codex の inbox の起動に足し、unit test で引数を確かめ、design に書く。
- **task 3334**: manual-smoke の起動の前後の人の設定の確かめで、書かれないことを確かめる。

### unresolved のとき（新しい版で測り直して結論が出なかったとき）

does_not_hold と同じ扱いで計画し直す。

### planner に当てはまるか

planner を Codex の対話の TUI で開いても、同じ起動の処理を通るので、同じ欄が同じ条件で書かれる。
止める引数と確かめの手順は inbox と同じものを使える。

## 残ったもの

scratch の dir（`$TMPDIR/dagq-worker-t3338-2d2tV9`。回ごとの `CODEX_HOME` と openai/codex の sparse の clone を含む）は残した。
測定の script・回ごとの前後の `config.toml` と diff・画面・`snap.log` は [codex-inbox-config-writes-spike/](codex-inbox-config-writes-spike/) に写した。
tmux の server は、各回の終わりに Codex が終わって無くなった。
