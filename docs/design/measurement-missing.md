---
id: design-measurement-missing
type: design
title: 計測の欠測と集約の契約
status: draft
created: 2026-11-09
scope: runtime
related:
  - design-measurement
  - adr-t1662-1
---

# 計測の欠測と集約の契約

[計測](measurement.md#共通の読み取り関数)の共通の読み取り関数が返す欠測と集約の約束を持つ。

値の無い窓は0や空でなく、理由つきの欠測として返す。

- 今の判定: 窓の終わりで猶予内の最も新しい成功した観測を持つ流れ1つの入力を窓の終わりで判定し、流れを足さず、開閉の記録から値を作らない。
  選べなければ「観測が無い」で、理由は`domain::live_alerts::Missing`のdoc commentが持つ。
- mainの履歴: 到達点（観測の到達時刻で、commitの時刻ではない）から猶予より後の窓は「記録が無い」（Gitの読み取りの失敗・supervisorの停止・到達点が古い・取り込みより前）。
- 着地の後の品質: 記録の前の着地は「記録より前」、読めない線の取り込みの前は「不明」で、窓の終わりまでの経過と到達を添える。
- 属性とfinding: 基準のeventより前の窓は「基準より前」。
- providerの控えと切り替え: 記録より前は「記録なし」、旧方式も畳む。
- Executionのcontext: 記録が無い（欄の無い前の記録）と未計測（nullと理由）を分ける。
- 失敗の原因の判定: 判定の記録の無いrunは不明で、環境が原因でないとは断定しない（ADR-t4163-1決定4）。
- token: Executionの終わった時刻（対話は区切りの時刻）で日に振り分け、actor × provider × modelで足す（[Execution tokens](execution-tokens.md)）。

