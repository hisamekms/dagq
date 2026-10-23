---
id: design-measurement-otel
type: design
title: 計測のOpenTelemetryとの対応（区間・step・資源・environment_attachedとspan・attribute・resource、idと時計、写さないもの）
status: draft
created: 2026-10-23
scope: runtime
related:
  - design-measurement
  - design-measurement-environment
  - adr-t1662-1
  - adr-t1662-3
  - adr-0033
---

# 計測のOpenTelemetryとの対応

計測の記録を外へ出すときにOpenTelemetryのspan・attribute・resourceへ写す対応で、[計測](measurement.md)の一部。
決めた理由はADR-t1662-3決定7（区間はspan、stepは子のspan、タグと属性はattribute）が持つ。

> **対応だけ**: OTLPで外へ出す仕組みはまだ無い。
> 正本は自前のストアのSSOT（EventStore・SessionStepStore・NodeSampleStore・RunResourceStore。区分は[計測](measurement.md#ssotとビュー)の「SSOTとビュー」）で、OTLPは記録の経路にしない。
> 写した先は台帳（LedgerStore）と同じくSSOTから作り直せるビューで、[ADR-0033](../adr/0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)のtracingの経路とも別。

## 対応の表

| 記録 | OpenTelemetry | 名前・時刻 | attributeとresource |
| --- | --- | --- | --- |
| taskの行 | trace（taskごとに1つ）の根のspan | `dagq.task`、行の最初の区間から最後の区間まで | `dagq.task.id` |
| runの行 | taskのspanの子のspan | `dagq.run`、claimからrunの終わりまで | `dagq.run.id`・`dagq.task.id`、claimの属性の組は`dagq.claim.<キー>` |
| runの区間（`run_phase_changed`から畳んだもの） | runのspanの子のspan | 工程の名前、区間の始まりと終わり | `dagq.blocker`・`dagq.holds`・`dagq.attempt`・`dagq.cause`・`dagq.blocked_by`、内側の層（step）の被覆率は`dagq.coverage` |
| `blocker: infra`の環境の待ち（`Phase::Environment`） | 上と同じ区間のspan | `environment_*`の工程の名前 | `dagq.blocker`は`infra` |
| unattributedの区間 | runのspanの子のspan | `unattributed` | `dagq.blocker`を持たない |
| runを持たない対象のsession（planner・job） | trace（対象ごとに1つ）の根のspan | `dagq.session`、登録からsessionの終わりまで | `dagq.target.kind`・`dagq.target.id`・`session.id` |
| runの上のsession | runのspanの子のspan | `dagq.session`、登録からsessionの終わりまで | `session.id`、stepの抜けの数は`dagq.session.missing_steps` |
| runの上のstep | stepの始まりを含むrunの区間のspanの子のspan（sessionのspanへはspan link） | stepの種類（`model_turn`・`tool_call`・`subagent`・`wait`）、sessionの始まり＋`offset_ms`から`duration_ms` | `session.id`と下の行と同じattribute |
| runを持たない対象のstep | sessionのspanの子のspan | stepの種類（`model_turn`・`tool_call`・`subagent`・`wait`）、sessionの始まり＋`offset_ms`から`duration_ms` | `dagq.step.seq`、`gen_ai.operation.name`（`chat`・`execute_tool`・`invoke_agent`、`wait`は無し）、`gen_ai.request.model`、`gen_ai.tool.name`、コマンドの形は`dagq.command.*`、`process.exit.code`、`dagq.command.background`、`dagq.command.output_bytes` |
| runの資源（区間ごとに畳んだ窓） | 区間のspanのattribute | 区間と同じ | `dagq.resources.cpu_seconds`・`.cpu_limit`・`.memory_max_bytes`・`.throttled_seconds`・`.cpu_pressure`・`.load_average`（外から測ったか中から測ったかはrunのspanの`dagq.resources.source`） |
| `environment_attached` | resource（その名乗りの下で測ったspanの） | — | `dagq.env.kind`、`host.id`（`node_id`）、`container.image.repo_digests`（`image_digest`）、`dagq.env.cpu_limit`、`dagq.env.mem_limit`、`host.arch`（`arch`） |
| NodeSampleStoreの行 | spanにしない（nodeのresourceのmetricのdata point） | sampleの時刻 | `host.id`（`node_id`） |

## 約束と落とし穴

- **idは記録から決まる**: traceのidは対象（taskか、runを持たない対象の種類とid）から、spanのidは記録の鍵（runのid、区間の始まりのevent id、`(session_id, seq)`）から決まる値にする。
  同じストアから写し直しても同じidになり、重複したspanを作らない。
- **時計はsupervisorのもの**: 区間の境目はsupervisorの時計のまま写す。
  stepはexecutorの時計を使わず、sessionの登録の時刻からの経過（`offset_ms`）で置く（[送る口](measurement.md#送る口)の「sessionの登録」）。
- **親は区間**: runの上のstepの親は、そのstepの始まりを含むrunの区間のspanで、sessionのspanではない（区間はspan、stepはその子のspan）。
  stepが区間の境目をまたいでも親は始まりの区間のままで、子が親より長いこともOpenTelemetryで許される。
  同じsessionのstepはspan linkでsessionのspanに結ぶ。
- **重なり**: runの区間は重ならず隙間が無いが、stepとsessionの内側の層は重なってよく、spanの子が重なることもOpenTelemetryで許される。
- **resourceは名乗りごと**: resourceは書き出しの単位ごとに1つなので、resumeや引き取りで名乗りが変わったrunのspanは、そのspanの始まりの時点の名乗りのresourceで分けて写す。
  名乗りの無いrunのspanは`dagq.env.kind`を持たない。
- **`host.arch`の値**: `arch`はRustの`std::env::consts::ARCH`の名前で、OpenTelemetryの値に直して写す（`x86_64`は`amd64`、`aarch64`は`arm64`、対応の無い名前はそのまま）。
- **失敗の印**: コマンドの終了コードは`process.exit.code`に写し、spanのstatusにはしない（testの失敗はstepの失敗ではない）。
- **写さないもの**: prompt・出力の本文・コマンドの全文は記録に無いので、どのattributeにも無い。
  コマンドの形は`[telemetry] command_detail`の詳しさで記録したまま写し、語のdigestはdigestのまま写して鍵は写さない（[計測のコマンドの形と分類](measurement-commands.md)）。
  tokenの数はExecutionの記録で、stepのspanには載せない。
