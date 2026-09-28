---
id: adr-t583-1
type: adr
title: 差し戻しのsessionが未closeのworker_questionを残したまま依頼の後にreceiptを書き直してidleになったら、質問のcloseを待たずにそのreceiptで判定し、slotの外の待ちもその書き直しで終える（ADR-0071決定2・16をamends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amends:
  - adr-0071 decision 2
  - adr-0071 decision 16
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
related:
  - adr-0071
  - design-supervisor-lifecycle-review
---

# ADR-t583-1: 差し戻しのsessionが未closeのworker_questionを残したまま依頼の後にreceiptを書き直してidleになったら、質問のcloseを待たずにそのreceiptで判定し、slotの外の待ちもその書き直しで終える（ADR-0071決定2・16をamends）

## Context

[ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)の決定16は、runにcloseされていない`worker_question`があるあいだ、差し戻し（Revise）の段がidleでも段を終えず`resume_timeout`も数えないと決め、task 238の実装はそのあいだreceiptもidleも見なかった。決定2はそれを前提に、`worker_question`を持つRevise / Resumeのslotの外の待ちを、書き直されたreceiptでは終えず、答えかcloseでだけ終えるとした。

そのため、workerが`dagq ask`の後に止まらずに作業を続け、receiptを書き直してidleになっても、人が質問に答えるかcloseするまで差し戻しは判定されず、着地が人の回答待ちで止まる。最初のsession（`SessionWatch`）は、receiptがあれば未closeの質問があっても`validating`へ進む。

## Decision

1. **差し戻しの段は、依頼より後に書き直されたreceiptの後のidleを、未closeの`worker_question`があっても判定する。**（決定16をamends）書き直しか不一致（`Rewritten` / `Mismatch`）は今の規則のまま。未closeの質問が止めるのは、receiptを書き直さずにidleになったときの段の終わりと`resume_timeout`だけで、これは決定16のまま。receiptを書き直していないうちはidleの判定もしない。書き直したreceiptより前の質問は、sessionが越えて進んだものとして以後この段を止めない（receiptの直し依頼の後の待ちと`resume_timeout`も止めない）。
2. **`worker_question`を持つ差し戻しのslotの外の待ちは、依頼より後に書き直されたreceiptで`session_moved`として終え、その場でslotに戻す。**（決定2をamends）戻ったslotでは1のとおり段が進む（書き直しの後のidleを待つあいだはslotを使う）。Resumeの段の待ちと判定は変えない（決定2・16のまま）。

## Alternatives

- **待ちは変えずに段の判定だけを変える**: 待ちに入ったrunは段のpollを呼ばないので、判定が効かず、差し戻しは人の答えまで止まったまま。
- **Resumeの段も揃える**: resumeの判定（`ResumeVerdict`）は古いreceiptの書き直しの依頼など別の規則を持ち、この問題の報告も無いので、この決定には含めない。
