//! `ask`: register a question for a person. Nothing is notified here: the
//! `watch --role inbox` that runs in the inbox's session tells the person
//! of each `ask_opened` it sees (ADR-t1433-1 decision 2), and the
//! supervisor, the queue service and the observer call no cmux. The
//! supervisor's own asks take this path too.

use anyhow::Result;
use serde_json::{Value, json};

use super::AskStore;
use crate::domain::{AskOutcome, HoldOutcome, NewAsk, NewHold};

/// Register `ask`. A repeated ask returns the open one (`created: false`).
pub fn ask(queue: &mut dyn AskStore, ask: NewAsk) -> Result<Value> {
    let outcome = queue.ask(ask)?;
    Ok(serde_json::to_value(&outcome)?)
}

/// Open the authentication or cost ask of `hold` with its run, or add the
/// run to the open one (ADR-0047 decision 42). Only a new ask is an
/// `ask_opened` for the inbox's watch to notify; a run that joins it
/// notifies nobody.
pub fn hold(queue: &mut dyn AskStore, hold: NewHold) -> Result<(HoldOutcome, Value)> {
    let outcome = queue.hold(hold)?;
    let mut value = serde_json::to_value(AskOutcome {
        ask: outcome.ask.clone(),
        created: outcome.created,
    })?;
    value["joined"] = json!(outcome.joined);
    Ok((outcome, value))
}
