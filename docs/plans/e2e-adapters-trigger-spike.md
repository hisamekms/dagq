---
id: plan-e2e-adapters-trigger-spike
type: plan
title: dagq.tomlの[e2e] pathsでhostのe2eを最も多く起こすsrc/infrastructure/adapters.rsを、runが触れた箇所が実プロセスの境目か純粋な判断かで数えたSpike
status: completed
created: 2026-10-26
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - plan-e2e-narrowing-measurement
  - adr-t963-1
  - adr-t1233-2
---

# dagq.tomlの[e2e] pathsでhostのe2eを最も多く起こすsrc/infrastructure/adapters.rsを、runが触れた箇所が実プロセスの境目か純粋な判断かで数えたSpike

[e2eを狭めた前後の測定](e2e-narrowing-measurement.md)の3章で、`src/infrastructure/adapters.rs`は`runs[].e2e`がpathsのruntimeのrun 235本のうち91本でhostのe2eを起こし、`[e2e] paths`のglobの中で最も多い。adapters.rsの純粋な部分をe2eの範囲の外へ移せばhostのe2eが減るかを、過去の着地の差分で数えたSpike（task 3494）。

## 要点

- **問い**: 「対象の期間に着地したpathsのruntimeのrunのうち、e2eの範囲のファイルとしてadapters.rsだけに触れたrun（adapters単独のrun）の25%以上は、adapters.rsの純粋な部分だけを変えている（純粋だけのrun）」。
- **判定: does_not_hold**。新しい順に60本のsampleのadapters単独のrun 21本のうち、純粋だけのrunは4本（19.0%）。25%に要る6本に2本足りず、sampleの未分類は0本。
- 減るのはこの4本のhostのe2eの830秒（0.23時間）で、期間の全体（4週）に外挿して約7本・約0.38時間。sampleの外の14本も参考に分類すると全35本で8〜9本（22.9〜25.7%）、1,200〜1,343秒（0.33〜0.37時間）で、閾値の根拠の「約23本・約0.9時間」の半分に届かない。
- 推奨: 今のままにする。分割もglobの変更も計画しない（5章）。

## 1. 環境

| 項目 | 値 |
| --- | --- |
| 対象のcommit | run branchの基点 `3e4bad44f858657dc5d08ba5936deaf6563d9166`（関数の数と行数の比較はこのcommitのadapters.rs、7,492行） |
| 期間 | task 966の着地（`run_integrated` event 40839、2026-09-28T23:59:58Z）の後から、event 421997（2026-10-26T06:00:03Z、task 3488の`run_integrated`）まで |
| 道具 | 固定バイナリ`~/.local/bin/dagq`（0.4.0-dev+fb928779d）の読み取り、git 2.39.3、Python 3.9.6（集計と関数への帰属のscriptは`$TMPDIR`に置き、commitしない）。macOS 14.7.4、arm64 |
| pathsのrun | 235本（`status`がintegrated・`areas`にruntime・`e2e`がpaths）。着地のcommitが見つからないrunは0本 |
| adapters.rsに触れたrun | 91本（各時点のglobに照らした範囲のファイルにadapters.rsを含むrun） |
| adapters単独のrun | 35本（範囲のファイルがadapters.rsだけのrun） |
| 分類したrun | 60本（91本が上限の60本を超えたので、着地の新しい順の60本をsampleにした。event 114555〜403086、2026-10-06〜10-25）。そのうちadapters単独のrun 21本 |
| 参考の分類 | sampleの外の31本（event 42339〜113644）も同じ基準で分類した。判定には使わない（4章の参考） |
| 使った時間 | 約1時間（上限2時間）。1回の読み取りで、周回は無い |

使った読み取りのコマンド:

- `dagq stats --since 40839 --until 421997 --full`（`next_cursor`が421997で、期間の全部が1回で返った）
- `dagq events --kind run_integrated --since 40839 --until 421997 --full`。この固定バイナリの`events`の`--since`・`--until`はUTCの時刻だけを受け、event idを拒む（`not a UTC time`）。代わりに`dagq events --kind run_integrated --after 40839 --limit 5000 --full`で読み、idが421997以下のものを取った。payloadの`commit`を着地のcommitとし、`run_id`で`runs[]`と突き合わせた
- `git show --stat <commit>`（触れたファイル）、`git show <commit>:dagq.toml`（その時点の`[e2e] paths`）、`git show <commit> -- src/infrastructure/adapters.rs`（hunk。関数への帰属には`-U0`と`git show <commit>:src/infrastructure/adapters.rs`の変更後の本体を使った）

## 2. 分け方

taskが置いた基準に従った。

- **境目**: 変更後の関数の本体が`std::process`・`tokio::process`・`Command`・`git2`・`launchctl`・`std::fs`による実ファイルの読み書きを直接呼ぶ関数、adapters.rsの中のそれらを呼ぶhelper（`capture`・`output`・`run_shell_to_log`・`GitRepository::command`・`git_root`など）を呼ぶ関数、それらを呼ぶtrait implのmethod。関数の呼び出しはadapters.rsの中で推移的に辿った。
- **純粋**: それ以外の関数・型・定数と、`#[cfg(test)]`の中のtest。`use`の行だけのhunkは数えない。境目と純粋の両方に触れたrunは境目。
- hunkの行は変更後のファイルで囲む関数に帰属し、関数の直前のdoc commentと属性はその関数に帰属した。doc commentだけを変えたhunkも、その関数に触れたものとして数えた。

基準を当てはめるときに決めたこと:

- `Repository for GitRepository`のmethodが、adapters.rsの子のmodule（`adapters/run_clones.rs`など）にある`GitRepository`の固有のmethodに委ねるだけのとき、委ね先がadapters.rsの`capture_bytes`・`git_root`を呼ぶので境目にした（task 2474のrun。その時点の`[e2e] paths`は`run_clones.rs`を含まず、このrunはadapters単独）。
- 範囲の外のmoduleのport（`RunFiles::write`の`LocalRunFiles`、`ClaudeTranscripts`）を通るだけの関数は、本体が上のどれも直接呼ばないので純粋にした（`turn_command`・`turn_session_exists`）。`CommandSpec`の組み立ては`Command`を呼ばないので純粋。
- 消した関数は変更前の本体で判じた（task 1990のrunの`Cmux::workspaces_described`は`workspace_listing`でcmuxを呼ぶので境目）。
- `libc::statvfs`・`libc::kill`だけを呼ぶ関数は基準の一覧に無い。sampleのadapters単独のrunにはこれだけで境目になるrunが無く、判定に効かない。参考の分類の1本（task 1371の`disk_space`）だけがこれに当たる（4章）。

## 3. adapters.rsに触れたrunごとの分類（sampleの60本、着地の新しい順）

host の e2e の秒は、event 56724 より後のrunは`runs[].land_phases.e2e`、それ以前は`runs[].work_breakdown.secs.e2e`（sampleは全部56724より後）。関数／implの列はtestでない関数を`名前@impl`で挙げ、関数の外の行（型・定数・doc）とtestは箇所の数だけを書く。

| run_id | task | 着地のcommit | adapters.rsで触れた関数／impl | 境目か純粋か | adapters単独か | hostのe2eの秒 |
| --- | --- | --- | --- | --- | --- | --- |
| `78b43ed8-0dfb-4a7a-8054-382474194863` | 3438 | `0ebc13d14` | `turn_command@AgentProvider for ClaudeCode`、test 7 箇所 | 純粋 | — | 66 |
| `6e47ad60-cb0d-48d1-a90c-82ac40451afd` | 3294 | `ab691a3f3` | `kept@crate::application::inherit::CarriedBranches for RunRepositories<'_>`, `prepare_write@crate::application::inherit::CarriedBranches for RunRepositories<'_>`, `ref_value@Repository for GitRepository`, `prepare_ref@Repository for GitRepository` ほか | 境目 | — | 130 |
| `cec9d767-2302-4598-9721-610aab13c87a` | 3388 | `4c8d3a8c8` | `main_checkout_of`, `main_checkout_from_files`, `main_worktree_of`、test 5 箇所 | 境目 | 単独 | 206 |
| `b534ae87-95b1-45db-91c2-7b0f8157bba4` | 3331 | `e8a64ff8c` | `inbox_command@AgentProvider for ClaudeCode`、test 1 箇所 | 純粋 | — | 178 |
| `d145ff6a-0b54-4be4-b71d-35c2c0bae406` | 3351 | `4517ec981` | `branch_head@GitRepository`, `branch_head@Repository for GitRepository` | 境目 | 単独 | 95 |
| `6cb7b07f-0053-4b0d-8741-d92b1a6eb001` | 3295 | `cdb48fe26` | `worktree_command`, `retry_raced`, `record_raced`, `clears_record`, `gone_is_removed`, `add_worktree@GitRepository`, `clear_failed_add@GitRepository`、test 18 箇所 | 境目 | 単独 | 264 |
| `b7571b1a-9df2-4873-92f0-772223961ec1` | 3105 | `8c512e8b0` | `session_start@ProcessControl for SystemProcesses`, `session_start` | 境目 | — | 706 |
| `a95098a1-4094-4060-9d15-8c2f08bb86bc` | 1673 | `1338f205f` | `step_reader@AgentProvider for ClaudeCode` | 純粋 | — | 290 |
| `b01ffc87-f3f5-46fe-9fe2-a21df3362471` | 1676 | `511197b00` | `run_shell_to_log`, `run_shell_to_log_started` | 境目 | — | 146 |
| `5d7c85f0-7b6d-4776-862f-5de99a621fa2` | 2978 | `62a359936` | `of@RunRepositories<'_>`, `carried_head@crate::application::inherit::CarriedBranches for RunRepositories<'_>`, `released_work@crate::application::inherit::ReleasedBranches for RunRepositories<'_>`、型・定数・doc など 6 行 | 境目 | 単独 | 99 |
| `150b164d-e5ca-4d23-b757-b0aa37acdb13` | 2474 | `92c450079` | `read_run_dir_file@Repository for GitRepository`, `write_run_dir_file@Repository for GitRepository`, `bundle_of@Repository for GitRepository` | 境目 | 単独 | 220 |
| `66a77d60-c657-4392-af33-519819ea08d2` | 2936 | `e6f99f985` | `worktree_command`, `record_raced`, `checkout_scratch@GitRepository`, `fresh_scratch@GitRepository`, `create_worktree@GitRepository`, `add_worktree@GitRepository`, `restore_worktree@GitRepository` ほか | 境目 | — | 299 |
| `809a26af-7ef4-4e5b-8333-39c191f8d114` | 2266 | `5255116cd` | `case_index`, `add_baseline_case_tree@GitRepository`, `add_baseline_case_tree@Repository for GitRepository`、型・定数・doc など 8 行 | 境目 | 単独 | 95 |
| `42f92bfb-f7ba-44f5-a824-bcad0809b9ab` | 2935 | `79077fa12` | `worktree_admin@GitRepository`, `worktree_admin@Repository for GitRepository`、型・定数・doc など 1 行 | 境目 | — | 323 |
| `59acccf7-9757-4dcd-8cf5-42e14468e4aa` | 2300 | `702d37d75` | `add_case_tree@GitRepository`, `add_case_base_tree@GitRepository`, `with_case_index@GitRepository`, `case_trees@GitRepository`, `add_case_base_tree@Repository for GitRepository` | 境目 | 単独 | 101 |
| `69e70141-160b-47f5-918f-0601c8520dfe` | 2230 | `cbb921c37` | `case_index`, `add_case_tree@GitRepository`, `case_trees@GitRepository`, `case_files@GitRepository`, `remove_case_tree@GitRepository`、test 2 箇所 | 境目 | — | 101 |
| `1c91814a-dccd-4827-9d89-511cf79b47dc` | 2969 | `bc60f4be3` | `is_dagq_source@Repository for GitRepository`, `file_in@crate::application::RepositoryReads for GitRepository` | 境目 | 単独 | 65 |
| `5c93082b-2f83-4831-ba4c-d84a1768ddf2` | 2110 | `fec43913c` | test 12 箇所 | 純粋 | 単独 | 215 |
| `177a0afe-af84-4676-adfd-b94b7dc571dc` | 2394 | `b37a1e53b` | test 1 箇所 | 純粋 | — | 228 |
| `f8864968-44d1-40b5-91f1-cf0f5914ae4e` | 2831 | `9970ec351` | `remove_untrusted`, `fresh_scratch@GitRepository`, `remove_scratch@GitRepository`, `fresh_scratch@Repository for GitRepository`, `remove_scratch@Repository for GitRepository` | 境目 | — | 96 |
| `990a09e8-8554-4c5f-8dbd-38ff22ff7532` | 2163 | `71c4e259c` | `inbox_settings`、test 6 箇所 | 純粋 | — | 179 |
| `3158f4fe-8a50-4001-8454-2424a9ab2814` | 2395 | `942bcf9ce` | `is_dagq_source@Repository for GitRepository`, `blobs_in@Repository for GitRepository`, `reachable_commit@Repository for GitRepository`, `is_ancestor@Repository for GitRepository` ほか | 境目 | — | 183 |
| `7ef12d10-f2c7-4619-a102-8c2030d01cfe` | 2310 | `48f82c4e7` | `run_shell_to_log` | 境目 | — | 180 |
| `f87a71a5-de6d-4bf0-b92c-9987016b4afa` | 2307 | `fa7b004fb` | `claude_trusts_repository` | 境目 | — | 214 |
| `a82f2469-ec69-4d66-aa3a-af13a626a49c` | 2624 | `75d821ea4` | `changed_hunks@GitRepository`, `changed_hunks@Repository for GitRepository` | 境目 | 単独 | 361 |
| `156dede0-0dbc-4721-acbc-dc0d76ce5786` | 2274 | `4e7ca45de` | `checkout_files` | 境目 | 単独 | 91 |
| `25914307-08b4-4edc-b9f3-64848295c760` | 2265 | `256e261c7` | `headless_command@AgentProvider for ClaudeCode`, `claude_tools`, `claude_edit_tools`、型・定数・doc など 1 行・test 5 箇所 | 純粋 | — | 295 |
| `fb02c9f3-9a3d-4751-aae8-3bb6fef663af` | 2623 | `5f9eccd86` | `verification_command`、test 5 箇所 | 境目 | — | 229 |
| `2e4f880a-ece9-45c2-8c1a-a381fc2db813` | 2470 | `65beeafe2` | `turn_session_exists@AgentProvider for ClaudeCode` | 純粋 | 単独 | 252 |
| `3e32493d-86db-432c-acff-10a5aa5a3443` | 1524 | `8116ecf0e` | `capture_with_input`, `worktree_diff@GitRepository`, `untracked_paths@GitRepository`, `git_operation@GitRepository`, `restore_worktree@GitRepository`, `move_worktree@GitRepository`, `discard_worktree@GitRepository` ほか | 境目 | — | 126 |
| `0e4eb30c-98ab-468d-bd3c-bbf43fb118fe` | 2515 | `3fd50f10e` | `repair_worktree_command@GitRepository`, `remove_worktree_command@GitRepository` | 境目 | 単独 | 739 |
| `f2b418b7-721f-45eb-99fc-839888ee36b1` | 2271 | `55bc336e9` | `host_versions`、型・定数・doc など 1 行 | 境目 | 単独 | 307 |
| `fa2bf7e9-5b3d-40e0-8c11-612621f26106` | 2413 | `af7ea3c22` | `turn_reader@AgentProvider for ClaudeCode` | 純粋 | — | 70 |
| `adb17cd9-b20c-4466-9eed-de5008c152b2` | 2262 | `c31ba83e0` | `review_command@AgentProvider for ClaudeCode` | 純粋 | — | 70 |
| `7bff181f-d1aa-432d-8905-0095726d73ae` | 1870 | `5d75c6a0a` | `reachable_commit@GitRepository`, `reachable_commit@Repository for GitRepository` | 境目 | 単独 | 129 |
| `5bbafed3-0fcb-4dc5-afeb-a1ced9be4d93` | 2161 | `388d1d1ea` | `load_average`, `launch_background@SessionWrappers for BackgroundSessions`、型・定数・doc など 2 行・test 3 箇所 | 境目 | — | 131 |
| `f778e5ae-1f4d-4871-9568-fa4ac89c7a28` | 2179 | `98a3b8649` | `inspect@GitRepository`, `command@GitRepository`, `repository_config@GitRepository`, `git_root@GitRepository`, `read_worktree@GitRepository`, `current_branch@GitRepository`, `head@GitRepository`, `is_ancestor@GitRepository` ほか | 境目 | — | 84 |
| `5ef16a4f-bff0-4442-9b0c-566859f332a4` | 2269 | `814477651` | `flaky_retry_vars`, `verification_command`、test 3 箇所 | 境目 | — | 72 |
| `fdeb7fd0-f8b8-43de-a3cb-c50d8801a0b8` | 1904 | `aae8adfec` | `agent_job_command@AgentProvider for ClaudeCode`、test 6 箇所 | 純粋 | — | 76 |
| `916ef878-df07-4087-94b7-0ee548bfa7bd` | 2160 | `e48134693` | `inbox_command@AgentProvider for ClaudeCode`, `inbox_settings` | 純粋 | — | 86 |
| `b318ee76-e502-420f-8751-f4cab9941527` | 1869 | `e0c36a4e7` | `expand_one_star`, `add_case_tree@GitRepository`, `remove_case_tree@GitRepository`, `add_case_tree@Repository for GitRepository`, `remove_case_tree@Repository for GitRepository`, `agent_job_command@AgentProvider for ClaudeCode` ほか | 境目 | — | 130 |
| `2e59cae0-77bd-4f3b-953d-ec247909c24d` | 1909 | `c419f0cab` | `main_history@GitRepository`, `main_commits@GitRepository`, `on_first_parent_line@GitRepository`, `has_commit@GitRepository`, `tree_paths@GitRepository`, `parse_main_log`, `main_commits@Repository for GitRepository` ほか | 境目 | 単独 | 228 |
| `4a04eaf5-ddd6-4d26-b31d-707395c4fb2c` | 1443 | `cde598ae5` | `process_alive`、型・定数・doc など 8 行・test 1 箇所 | 境目 | — | 312 |
| `11e6ddd4-ab7b-4ad1-8bda-4448e2d92505` | 2117 | `c35ee27c9` | 型・定数・doc など 1 行・test 3 箇所 | 純粋 | — | 256 |
| `df3a1db6-2573-4d63-867e-23adc5e513f6` | 1889 | `66ae577c5` | `headless_command@AgentProvider for ClaudeCode`, `job_reply@AgentProvider for ClaudeCode`, `job_session@AgentProvider for ClaudeCode`, `review_command@AgentProvider for ClaudeCode`、test 7 箇所 | 純粋 | — | 106 |
| `b405848b-d232-4d81-b1dd-e8530232db60` | 2021 | `f99a2b0cb` | `background_wrappers`, `send_text@WorkspaceBackend for Cmux`, `send_key@WorkspaceBackend for Cmux`, `capture@WorkspaceBackend for Cmux`, `close@WorkspaceBackend for Cmux`, `set_color@WorkspaceBackend for Cmux`, `set_status@WorkspaceBackend for Cmux` ほか | 境目 | — | 388 |
| `b5cd0c9c-5f7e-4268-ac2f-0b410b3163b7` | 1490 | `bac0cdd12` | `headless_command@AgentProvider for ClaudeCode`, `job_reply@AgentProvider for ClaudeCode`, `job_session@AgentProvider for ClaudeCode`, `review_command@AgentProvider for ClaudeCode`、型・定数・doc など 1 行・test 11 箇所 | 純粋 | — | 100 |
| `b2375b92-a2a7-4525-bb24-497c801f157c` | 1993 | `2ca5227b8` | `planner_command@AgentProvider for ClaudeCode`, `review_command@AgentProvider for ClaudeCode`, `headless_worker_settings`, `agent_settings`, `write_settings`、型・定数・doc など 1 行・test 12 箇所 | 純粋 | — | 325 |
| `46e9d7c4-279e-4849-ae8f-5a8fcec69d82` | 2086 | `6db427d03` | `verification_command`、test 6 箇所 | 境目 | — | 101 |
| `1fddba59-45d9-4440-9a69-fabd361f13c5` | 2103 | `f78dd39b5` | `headless_command@AgentProvider for ClaudeCode`, `review_command@AgentProvider for ClaudeCode`, `headless_worker_settings`, `headless_required_settings`, `claude_agent_job_tool_args`、型・定数・doc など 1 行・test 21 箇所 | 純粋 | — | 100 |
| `2249516d-d6f4-40ff-913e-c01536db0d94` | 1990 | `860ac29ee` | `（削除）workspaces_described@WorkspaceBackend for Cmux` | 境目 | — | 116 |
| `dcf01067-0489-40cd-9465-04f40fac86ca` | 2085 | `f394624b1` | `parse_etime` | 純粋 | 単独 | 231 |
| `8f3514c0-f119-4c60-88e5-532b3832bf2e` | 2079 | `3c1968491` | `provider_executable_on`, `is_runnable`, `provider_executable`, `replaces`, `provider_at_entry`, `claude_at_entry`, `relocated`, `relocated_executable@AgentProvider for ClaudeCode`、型・定数・doc など 1 行・test 20 箇所 | 境目 | — | 86 |
| `2551405d-3809-4a7b-9dab-ef7c1a94afa2` | 1690 | `8e7542f0c` | `claude_config_dir`, `worker_plugin_hash`, `plugin_files`, `blobs_in@GitRepository`, `blobs_in@Repository for GitRepository`、test 12 箇所 | 境目 | 単独 | 97 |
| `9eea1f21-1e4e-4776-92f1-d34dbc074527` | 1442 | `f5ec48f10` | `no_cmux`, `launch_background@WorkspaceBackend for BackgroundSessions`, `preflight@WorkspaceBackend for BackgroundSessions`, `preflight_detached@WorkspaceBackend for BackgroundSessions`, `call_timeout@WorkspaceBackend for BackgroundSessions` ほか | 純粋 | — | 976 |
| `e2f02d00-eb18-420e-9e9e-672fce76038f` | 1962 | `244701e37` | `of@RunRepositories`, `carried_head@crate::application::inherit::CarriedBranches for RunRepositories`, `keep@crate::application::inherit::CarriedBranches for RunRepositories`、型・定数・doc など 5 行 | 境目 | 単独 | 268 |
| `9dfa3613-bd6a-457e-8f57-a5ca0439bf23` | 1952 | `403e3824a` | `turn_command@AgentProvider for ClaudeCode` | 純粋 | 単独 | 132 |
| `5ca52e56-6c9f-4b6c-9d4e-d971eee4a596` | 1955 | `680d30e14` | `merge_conflicts@GitRepository` | 境目 | — | 171 |
| `21291c4f-c263-4b5f-aa52-08399eac5863` | 1950 | `63ebb3e6f` | `claude_trusts_repository`, `stop_hook_settings`、型・定数・doc など 1 行 | 境目 | 単独 | 197 |
| `59f842a5-d062-465a-8648-fd00deba193e` | 1441 | `c01ddb655` | `command@AgentProvider for ClaudeCode`, `resume_command@AgentProvider for ClaudeCode`, `planner_command@AgentProvider for ClaudeCode`, `review_command@AgentProvider for ClaudeCode`, `agent_settings`, `write_settings` ほか | 純粋 | — | 90 |

内訳（sampleの60本）: 境目39本・純粋21本・未分類0本。adapters単独の21本では境目17本・純粋4本・未分類0本。純粋の21本の多くは`AgentProvider for ClaudeCode`のcommandとsettingsの組み立て（`headless_command`・`review_command`・`inbox_settings`など）とtestで、そのうち17本は他のglobのファイル（`claude.rs`・`codex.rs`・`headless_session.rs`など）にも触れている。

## 4. 減るrunの本数とhostのe2eの時間

taskが置いた式で、減るのはadapters単独かつ純粋だけのrunだけとし、hostのe2eはそのrunの実測の合計で出す。

| 数え方 | adapters単独 | 純粋だけ | 割合 | 減るhostのe2e（実測の合計） | 値の無いrun |
| --- | --- | --- | --- | --- | --- |
| sample（判定に使う） | 21 | 4（task 2110・2470・2085・1952） | 19.0% | 830秒（0.23時間） | 0本（143秒で埋めた値も830秒） |
| sampleの割合で期間の35本に外挿 | 35 | 約6.7 | 19.0% | 約1,380秒（約0.38時間、6.7本×4本の実測の平均207.5秒） | — |
| 参考: sampleの外の14本も分類した全35本 | 35 | 8（上の4本とtask 1907・1709・1615・1373） | 22.9% | 1,200秒（0.33時間） | 0本 |
| 参考: 上で`libc::statvfs`だけの`disk_space`（task 1371）を純粋に数えたとき | 35 | 9 | 25.7% | 1,343秒（0.37時間） | 0本 |

- 純粋だけのrunの中身は、testだけ（task 2110・1907・1709・1615）、`parse_etime`（2085）、`headless_worker_settings`（1373）、doc commentかportを通す`AgentProvider for ClaudeCode`のmethod（2470・1952）。
- 比べると、adapters.rsに触れた91本のhostのe2eは合わせて17,673秒（4.9時間）、adapters単独の35本は6,353秒（1.8時間）。減る量（sampleの830秒〜参考の1,343秒）は91本の5〜8%で、pathsの群の期間の合計（52,503秒、14.6時間）の1.6〜2.6%。
- 期間の後ろ（sample、10-06〜10-25）ほど純粋だけのrunの割合は低い（sampleの外の14本では4〜5本、29〜36%）。cmuxの撤去とsettingsの組み立ての変更が多かった時期が外れ、今の傾向は19%に近い。

### (a) 純粋な部分を範囲の外へ移す案と、(b) globを境目のファイルだけに狭める案

どちらもe2eが要らなくなるのは上の同じrunの集合で、減る量は同じ。違いは移す関数の数と行数（基点のcommitのadapters.rs、7,492行）。

| 案 | 移すもの | 関数の数 | 行数 | 衝突のしやすさ |
| --- | --- | --- | --- | --- |
| (a) 純粋な部分を`src/infrastructure/adapters/`の下の範囲の外の新しいファイルへ移す | testでない純粋な関数（free 47・`AgentProvider for ClaudeCode` 17・`GitRepository` 4・`SessionWrappers for BackgroundSessions` 4）と、それを確かめる`#[cfg(test)]`のtest | 72 | 約820行（free 555・ClaudeCode 223・ほか45）＋testの一部（testのmoduleは2,624行） | `AgentProvider for ClaudeCode`のtrait implは1つのblockに置くので、17のmethodは本体を新しいファイルのfree関数に出してimplから呼ぶ形になる。境目の`GitRepository`には触れない |
| (b) adapters.rsを境目ごとのファイルに分け、`[e2e] paths`をそのファイルだけにする | 境目の関数（`GitRepository` 82・`Repository for GitRepository` 67・free 45・`ProcessControl for SystemProcesses` 12・ほか25）と、それらが使う型・定数 | 231 | 約2,820行＋型・定数（関数の外の行は約1,220行） | 着地の多いrunが触る`GitRepository`と`Repository for GitRepository`（149の関数）を動かすので、(a)の3倍余りの行が動き、並行するrunとの衝突が多い |

## 5. 判定と推奨

### 判定

**does_not_hold**。sampleのadapters単独のrun 21本のうち純粋だけのrunは4本（19.0%）で、25%（6本）に2本足りない。sampleの未分類は0本で、分類したadapters単独のrunは20本以上ある（21本）ので、taskが置いたunresolvedの条件（分類できなかったadapters単独のrunが判定を変える本数あること、分類したadapters単独のrunが20本未満であること）のどちらにも当たらない。2章の決めごとのうち、task 2474の委ねるだけのmethodを純粋に数えても5本（23.8%）で25%に届かない。範囲の外のportを通る関数を純粋にした決めごと（task 2470・1952）は境目の側に倒すと純粋だけのrunを2本に減らし、判定を変えない。

参考の全35本の分類では8〜9本（22.9〜25.7%）で、閾値の上下は`libc::statvfs`だけを呼ぶ1本の扱いで変わる。ただしそのときでも減るのは9本・0.37時間で、問いの25%の根拠に置いた「約23本・約0.9時間」の半分に届かない。sampleの判定と、減る量が閾値の根拠を下回ることの両方から、分割に見合わない。

### 推奨

今のままにする。adapters.rsの分割も`[e2e] paths`のglobの変更も計画せず、この問いでの再計画（さらなるSpike）を止める。理由:

- adapters.rsに触れたrunの大半（sampleの65%）は境目に触れ、移してもe2eが要る。純粋に触れたrunの多くは他のglobのファイル（`claude.rs`・`codex.rs`・`headless_session.rs`など）にも触れ、adapters.rsから純粋な部分を外してもe2eが残る。
- 減るのは4週で0.23〜0.38時間（pathsの群のhostのe2eの2%前後）で、(a)でも約820行と`AgentProvider for ClaudeCode`の17のmethodの組み替えが要り、7,492行のファイルを動かす衝突と手間に見合わない。
- 境目だけを範囲に置く方針（ADR-t963-1決定2・3、ADR-t1233-2）は変えない。この結果はその方針の当てはめを変える必要を示さない。

## 6. 再現の方法

1. `dagq stats --since 40839 --until 421997 --full`の`runs[]`から、`status`がintegrated・`areas`にruntime・`e2e`がpathsのものを取る。
2. `dagq events --kind run_integrated --after 40839 --limit 5000 --full`の、idが421997以下のeventのpayloadの`commit`を`run_id`でrunに対応させる。
3. `git show --stat <commit>`のファイルを`git show <commit>:dagq.toml`の`[e2e] paths`に照らし、adapters.rsに触れたrunとadapters単独のrunを分ける（`/**`で終わるglobは前方一致、ほかは`*`がdirectoryを越えない一致）。
4. 着地の新しい順に60本を取り、`git show -U0 <commit> -- src/infrastructure/adapters.rs`のhunkの行を`git show <commit>:src/infrastructure/adapters.rs`の関数に帰属し、2章の基準で分ける。
