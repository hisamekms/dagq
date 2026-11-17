---
id: design-supervisor-lifecycle-local-config
type: design
title: "期限つきの運用設定の層"
status: current
created: 2026-11-17
scope: runtime
related:
  - adr-t5108-1
  - design-supervisor-lifecycle-run-environment
---

# 期限つきの運用設定の層

main checkoutの`dagq.toml`の隣の`dagq.local.toml`を、commitしない期限つきの運用設定として重ねる（[ADR-t5108-1](../../adr/2026-11-16-t5108-1-expiring-uncommitted-operational-config-layer.md)）。
設定の読み方の全体は[Run environment](run-environment.md)、読むcheckoutは[main checkoutの決め方](run-environment.md#main-checkoutの決め方)が持つ。

## 重ね方

- 期限を必須とし、稼働中に読み直せる運用のknobだけをallowlistで許し、層に誤りや許さないkeyがあれば全体を使わない。
- 着地の前の関門を変える設定は、この層では許さない。
- 読み直すたびに効き、supervisorでは次のpassから変わる。
- 期限切れ・拒否・削除でも次のpassからbaseへ戻り、起動のflagは優先する。
- commitしたtree・run worktree・deploymentのremote rulesには重ねない。
- 読みと重ねる入口は`run_env::inspect_local_config`で、allowlistと期限の上限は`LOCAL_TABLES`と`LOCAL_CONFIG_MAX_SECS`のdoc commentが持つ。

## 記録と見え方

- supervisorは各passで層を読んだ結果を前のpassと比べ、変わったときだけqueueのeventに1件残す。
  層の下でcommitした値が着地で変わったことも1回記録する（変化の種類は`domain::local_config`のdoc comment）。
  そのpassで使う値は記録と同じ読みから適用し、層で変わった値も、値ごとに分けず同じ1件に含める。
  serviceが答えなかった記録は再送に任せ、同じ変化を記録し直さない。
  記録は`dagq events`で読み、kindと欄は`domain::local_config`のdoc comment、記録するのは`Supervisor::record_local_layer`が持つ。
- `dagq status`はどのroleでも、`dagq doctor`も、今の層を出す。
  今効いている層の値・期限・拒否の理由を示す（項の形は`health::local_config`のdoc comment）。
  commitした値と同じkeyと、層が効き始めた後にcommitした側が変わったkeyは、食い違いとして示す。
  `doctor`はGitが無視していない層を、Gitのignoreの規則と優先順位で判じて警告する（`run_env::local_config_ignored`）。
  ignoreのファイルやGitの設定が読めずに判じられないときは、その理由を示す。
  期限の前に知らせ始める閾値は`LocalLayerRead::report`のdoc commentが持つ。
- 監査の設定の記録（設定のsnapshotと`up`・`down`・監査の開始の記録）は、層が効いている間はそのファイルのhashも持つ。
