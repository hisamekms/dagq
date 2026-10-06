---
id: design-test-fixtures
type: design
title: Test fixture templates
status: current
created: 2026-10-06
updated: 2026-10-06 # task 1886
last_verified: 2026-10-06 # task 1886
scope: operations
tags:
  - testing
  - ci
related:
  - development-testing
  - design-linux-ci
  - design-ci-failure-issues
---

# Test fixture templates

integration test の fixture の queue・repository・shell の stub は、`tests/common/template.rs` の template を 1 度だけ disk に作り、各 fixture にコピー（stub は hard link）する（task 1044・1060）。fixture ごとに migration を全部当てたり `git` を何度も打ったりしないため。この文書は template の作り方と、CI の cache が中身の無い template を残して main の CI を 2026-09-29 から赤くした原因と直し方（task 1886、goal 144）を持つ。

## template の作り方

- 置き場所: `CARGO_TARGET_TMPDIR` の下の `fixture-templates/`（`target/tmp/fixture-templates`、`cargo llvm-cov` では `target/llvm-cov-target/tmp/fixture-templates`）。nextest は test ごとに process を分けるので、template は process ではなく disk で共有する。
- 名前: 作る材料から決める。queue は `queue-f<FORMAT>-v<schema の版>-<migration の hash>.db`、repository は `repo-f<FORMAT>-<seed の hash>`、stub は `script-f<FORMAT>-<中身の hash>`。migration や seed が変われば別の名前の template を新しく作る。
- file lock と rename: `made()` が作る。template が無ければ `fixture-templates/<name>.lock` に `flock` を取り、`<name>.building` の空のディレクトリで作り、`<name>` に rename する。半分作った template を他の process が見ることは無い。死んだ process が残した `.building` は lock の下で消して作り直す。
- `FORMAT`: 作り方（手順や `init` 自体）が migration と seed の外で変わったときに上げ、古い template を名前で避ける。task 1886 で 2 に上げ、完成の印の無い古い template を名前でも使わない。
- 完成の印: `made()` は make の後・rename の前に、最後に `.complete` を書く。template の完成は `.complete` があることで判定する（lock の外の速い経路でも lock の中でも）。印の無い `<name>` のディレクトリが残っていれば、lock の下で `remove_dir_all` で消して作り直す。印のある template は消さないので、読んでいる process の template を消すことは無い。
- repository の確かめ: `template::git_repository` が、`git_executable` の解決した git で `git -C <dir> rev-parse --absolute-git-dir` を打ち、`<dir>/.git` 自身であることを確かめる（`GIT_CEILING_DIRECTORIES` で上のディレクトリを探させない。template は checkout の中の `target/tmp` にあり、探させると外の repository で通ってしまう）。repository の template を作った直後（印を書く前。通らなければ印を書かずに止まる）と、`repository()` のコピーの後のコピー先の 2 か所で確かめ、通らなければ確かめた dir・git の path・git の stderr を含む message で panic する。
- test から root を差し替える: `template::with_root` がその thread の間だけ template の root を替える。template 自体の test（`tests/it/fixture_templates.rs`）は一時ディレクトリの root で template を作って壊し、本物の共有の template に触らない。

## CI の cache と中身の無い template

- 症状: main の CI は 2026-09-29（d12de1f1）から赤く、2026-10-05 の run 37381283097 では Linux 2,883 本中 916 本、macOS 685 本が落ちた。主な panic は `script()` の `hard_link(template.join("stub"), …)` の NotFound と、repository の template のコピー先での git の「fatal: not a git repository」。
- 原因: CI の `Swatinem/rust-cache@v2` は `target/` を cache し、保存の前の cleanTargetDir が profile 以外のディレクトリ（`target/tmp` を含む）のファイルを全部消してディレクトリを残す。中身の無い template（空の `.git/` だけの repository、`stub` の無いディレクトリなど）が cache に入り、次の回の `made()` が `template.exists()`（ディレクトリがあるか）だけで完成と判定して使った。
- 固定されたこと: rust-cache は cache の key が当たると保存しないので、悪い cache が一度入ると、落ちた job はそれを上書きせず、同じ key の間は毎回その空の template を復元した。cache の作成時刻と成功から失敗への変化の時刻が一致した（checks の cache …-2ad26564 は 2026-09-29 00:22Z、linux の …-ef1c0efb は 2026-10-04 22:20Z）。
- git の版の違いは原因ではなかった: macOS の job の git（Homebrew の 2.55.0）と手元（Apple Git 2.39）の違いを疑った task 1824 は、この task の重複として cancel した。Linux の job も同じ cache で落ちた。
- 直し方: 上の完成の印で判定し、印の無い template を作り直す。ファイルだけ消された template（rust-cache の後始末の形）から script・repository・queue の template を作り直して正しく働くことを `fixture_templates::templates_emptied_by_a_cache_cleanup_are_made_again` が、確かめの message を `fixture_templates::the_repository_check_names_the_dir_the_git_and_its_stderr` が確かめる。
- 姉妹の task: CI の workflow 側で `target` の tmp（template の置き場所）を cache から外すことは goal 144 の姉妹の task が `.github/workflows/` で持つ。この文書の直し方は cache に何が入っていても template を正しく使う側の備え。
