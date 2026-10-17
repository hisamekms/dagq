---
id: adr-t2596-1
type: adr
title: commitの接頭辞はfeat・fix・docs・test・refactor・perf・choreの7つで、run branchのcommitはprekとcommittedのcommit-msgのhookとrunのreviewのprogramが、mainのsquash commitはtaskのtitleの検査が弾き、runのreviewのLLMは書式を見ない
status: accepted
created: 2026-10-17
updated: 2026-10-17
accepted_on: 2026-10-17
owners:
  - hisamekms
tags:
  - conventions
  - review
related:
  - adr-t2179-1
  - adr-t2257-1
  - adr-t1967-1
---

# ADR-t2596-1: commitの接頭辞は7つで、hook・reviewのprogram・taskのtitleの検査が弾く

## Context

[文書の規則](../development/documents.md)の「commit」はrun branchのcommitに`feat:`・`fix:`・`docs:`・`test:`だけを許し、ADRを持たなかった。
mainのsquash commitの件名はtaskのtitleで、直近のmainは`runtime:`・`config:`・`measure:`などが多く、規則と食い違っていた。
検査は機械に無く、runのreviewのLLMが読んでいた。10/16以降のrunのreviewの差し戻し71件のうち24件がrun branchのcommitメッセージを指摘し、少なくとも17件は書式だけで差し戻され、そのたびにworkerの手直しとreviewの流し直しを待った。
制御側のgitはhookを走らせない（[ADR-t2179-1](2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定8）。runtimeはprojectに固有の道具と値を固定しない（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定1）。

## Decision

1. **接頭辞は`feat`・`fix`・`docs`・`test`・`refactor`・`perf`・`chore`の7つで、run branchのcommitとmainのsquash commitの両方に当てる。**
   人の決定（request 116）である。
2. **一覧の正本はcommit lintの設定1か所で、hookは`.githooks/commit-msg`を入口にprekのcommit-msgのstageからcommittedを流す。**
   一覧はcommittedの設定（`.config/committed.toml`）に置き、他の検査はそこを読む。
   lintはNodeの要らないRustの単体のバイナリcommittedで、prekの設定はtrackedの中身だけで流すlocalのhookにする（networkから取らない）。
   入口はrepositoryに置く`.githooks/commit-msg`で、cloneごとに1回の`core.hooksPath=.githooks`が各worktreeのtopから解決されるので、hostのworkerのrunのworktreeにも届き、[ADR-t1967-1](2026-10-07-t1967-1-adr-index-generated-from-frontmatter.md)の索引のhookと並ぶ。
   `prek install`は使わない。prekもcommittedも無いhostではhookは通すだけにする。
3. **hookは早い知らせで、関門はrunのreviewのprogramである。**
   programはcommittedに頼らないshで、mainとのmerge base..HEADのcommitを同じ一覧で検査し、合わないrunはLLMのreviewの前にworkerに返る。
   `--no-verify`やhookの無いhost・cloneのcommitもここで返る。
4. **mainはhookでなくtaskのtitleで揃える。**
   制御側のgitはhookを走らせない約束（ADR-t2179-1決定8）を変えないので、squash commitの件名になるtaskのtitleを`dagq.toml`の`[tasks]`の接頭辞の一覧で`dagq lint`とsubmitが検査する。
   runtimeは仕組みだけを持ちprojectの値を読み、値はprojectの`dagq.toml`に置く（ADR-t2257-1決定1）。
5. **runのreviewのLLMはcommitメッセージの書式を判定しない。**
   receiptの`summary`が何をなぜ変えたかを持つことは判定に残す。
6. **containerのworkerのcloneにはprojectのContainerfileがhookを入れる。**
   imageにprekとcommittedを入れ、cloneでも同じ`.githooks/commit-msg`が流れる形にする。runtimeはhookを知らない（ADR-t2257-1）。

## Alternatives

- commitlintの本家: Nodeが要り、Rustだけのhostとcontainerのimageに道具が増える。
- `cog verify`（cocogitto）: 単体のバイナリだが、型の一覧の設定がversionの管理の設定と一緒になり、commit lintだけを置くには重い。
- runtimeがmainのメッセージに接頭辞を足す: runtimeがprojectの規則を持つことになり、ADR-t2257-1に反する。
- integrateがsquashのmessageにlintを流す: 制御側でtrackedの中身が選ぶプログラムを走らせることになり、ADR-t2179-1決定8に反する。titleは登録の時に直せるので、着地で止めるより早い。
- main checkoutで`prek install`を打って`.git/hooks`にhookを作る: `core.hooksPath`を置いたcloneでは使われず、他のworktreeにも現れない。

## Consequences

- 書式だけの差し戻しはLLMのreviewの前のprogramで返り、reviewの手間と待ちが減る。
- 人の作業はhostにprekとcommittedを入れることと、cloneごとの`core.hooksPath`の設定だけになる。入れないhostでもprogramとtitleの検査が関門として残る。
- 一覧を変えるときはcommittedの設定と`dagq.toml`の`[tasks]`の一覧の2か所を合わせる（runtimeがprojectのlintの設定を読まないため）。
