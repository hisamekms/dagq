//! What a run's events settle after its claim about the provider it
//! worked on (ADR-t1662-1 decisions 23 and 25): the provider that did its
//! work in the end, whether it worked on Codex, and the model its first
//! Codex turn reported. `stats` (its `MeasureTrack`) and the ledger's run
//! fold read them by this one rule.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{RunEvent, event_kind};

/// The provider of a turn recorded without one, when the run named none
/// either.
const UNKNOWN: &str = "unknown";

/// The facts after the claim, as the ledger's run row keeps them beside
/// the claim's attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AfterClaim {
    /// The `to` of the run's last `provider_switched` (a move at the claim
    /// included), else its claim's `provider`.
    pub last_provider: Option<String>,
    /// It worked on Codex: claimed there, moved there, or a turn ran there.
    pub on_codex: bool,
    /// The model the first Codex turn that named one reported; none for a
    /// run that did not work on Codex.
    pub codex_model: Option<String>,
}

/// The facts after the claim as a run's events come in, oldest first.
#[derive(Debug, Clone, Default)]
pub struct AfterClaimTrack {
    claimed: bool,
    /// The first claim's `provider`, and the `to` of the last
    /// `provider_switched` (which may come before the claim: a move at the
    /// claim is recorded first).
    claim_provider: Option<String>,
    switched_to: Option<String>,
    on_codex: bool,
    codex_model: Option<String>,
}

impl AfterClaimTrack {
    /// Take in `event`. For a `turn_finished`, the provider that ran the
    /// turn: its own `provider`, else the one the run was on then.
    pub fn observe(&mut self, event: &RunEvent) -> Option<String> {
        let payload = &event.payload;
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
        match event.kind.as_str() {
            event_kind::RUN_CLAIMED if !self.claimed => {
                self.claimed = true;
                self.on_codex |= payload["provider"] == "codex";
                self.claim_provider = text("provider");
                None
            }
            event_kind::PROVIDER_SWITCHED => {
                self.switched_to = text("to");
                self.on_codex |= payload["to"] == "codex";
                None
            }
            event_kind::TURN_FINISHED => {
                let provider = text("provider")
                    .or_else(|| self.switched_to.clone())
                    .or_else(|| self.claim_provider.clone())
                    .unwrap_or_else(|| UNKNOWN.to_owned());
                if provider == "codex" {
                    self.on_codex = true;
                    if self.codex_model.is_none() {
                        self.codex_model = text("model");
                    }
                }
                Some(provider)
            }
            _ => None,
        }
    }

    /// The facts of the events taken in so far.
    pub fn facts(&self) -> AfterClaim {
        AfterClaim {
            last_provider: self
                .switched_to
                .clone()
                .or_else(|| self.claim_provider.clone()),
            on_codex: self.on_codex,
            codex_model: self.codex_model.clone().filter(|_| self.on_codex),
        }
    }

    /// The facts of all of `events`.
    pub fn of(events: &[RunEvent]) -> AfterClaim {
        let mut track = Self::default();
        for event in events {
            track.observe(event);
        }
        track.facts()
    }
}
