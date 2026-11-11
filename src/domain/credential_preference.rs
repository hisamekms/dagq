//! Which Claude credentials a team deployment may use, and what each person
//! offers (ADR-t3987-1 decision 8). The deployment's part is two settings of
//! the deployment config ([`DEPLOYMENT_NAMES`], read by
//! `infrastructure::deployment_config`); a person's part is one row bound to
//! their registry entry (`member_credentials`). Neither is in `dagq.toml` or
//! `host.toml`. Choosing the credential of an assignment from these values is
//! the assignment's, not this module's: here are the values, their reading
//! and who may change a person's row.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::oidc::OidcIdentity;

/// Whether the deployment allows the team's API key: `true` or `false`.
/// Absent, it follows whether [`TEAM_API_KEY_SECRET`] is set. `true` without
/// the secret refuses startup, naming both settings and neither value.
pub const TEAM_API_KEY_ENV: &str = "DAGQ_CLAUDE_TEAM_API_KEY";
/// The deployment's default order when a job could use both a person's own
/// subscription and the team's key: `personal_first` (the default) or
/// `team_first`. A person's own [`Preference::prefer`] goes before it.
pub const PREFER_ENV: &str = "DAGQ_CLAUDE_PREFER";
/// The team's API key itself, a secret held by the control side only
/// (ADR-t3985-1 decision 8 (i)).
pub const TEAM_API_KEY_SECRET: &str = "DAGQ_ANTHROPIC_API_KEY";
/// The settings this module reads from the deployment config.
pub const DEPLOYMENT_NAMES: [&str; 2] = [TEAM_API_KEY_ENV, PREFER_ENV];

/// Which credential goes first when both could run a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    PersonalFirst,
    TeamFirst,
}

impl Order {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "personal_first" => Some(Self::PersonalFirst),
            "team_first" => Some(Self::TeamFirst),
            _ => None,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PersonalFirst => "personal_first",
            Self::TeamFirst => "team_first",
        }
    }
}

/// Whether a person offers their own subscription for their own jobs on
/// their own unshared runners: `personal` or `none`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Personal,
    None,
}

impl Offer {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "personal" => Some(Self::Personal),
            "none" => Some(Self::None),
            _ => None,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::None => "none",
        }
    }
}

/// The deployment's part, read and checked at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deployment {
    /// The team's API key may be chosen (and its secret is present).
    pub team_api_key: bool,
    /// The order of a person who chose none.
    pub prefer: Order,
}

impl Deployment {
    /// The deployment config's values, or the names of the settings that
    /// refuse startup (never a value). `key` is whether
    /// [`TEAM_API_KEY_SECRET`] is set.
    pub fn read(
        allow: Option<&str>,
        prefer: Option<&str>,
        key: bool,
    ) -> Result<Self, BTreeSet<&'static str>> {
        let mut errors = BTreeSet::new();
        let team_api_key = match allow {
            None => key,
            Some("true") if key => true,
            Some("true") => {
                errors.extend([TEAM_API_KEY_ENV, TEAM_API_KEY_SECRET]);
                false
            }
            Some("false") => false,
            Some(_) => {
                errors.insert(TEAM_API_KEY_ENV);
                false
            }
        };
        let prefer = match prefer {
            None => Order::PersonalFirst,
            Some(text) => Order::parse(text).unwrap_or_else(|| {
                errors.insert(PREFER_ENV);
                Order::PersonalFirst
            }),
        };
        if errors.is_empty() {
            Ok(Self {
                team_api_key,
                prefer,
            })
        } else {
            Err(errors)
        }
    }
}

/// A person's row. A registered person with no row reads as
/// [`Preference::NONE`]: they offer nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preference {
    pub offer: Offer,
    /// `None` follows the deployment's [`Deployment::prefer`].
    pub prefer: Option<Order>,
}

impl Preference {
    pub const NONE: Self = Self {
        offer: Offer::None,
        prefer: None,
    };

    /// The row, or what a person without one reads as.
    pub fn of_row(row: Option<Self>) -> Self {
        row.unwrap_or(Self::NONE)
    }

    /// This preference with the fields `update` names replaced.
    pub fn updated(self, update: Update) -> Self {
        Self {
            offer: update.offer.unwrap_or(self.offer),
            prefer: update.prefer.unwrap_or(self.prefer),
        }
    }

    pub fn to_json(self) -> Value {
        json!({
            "offer": self.offer.as_str(),
            "prefer": self.prefer.map(Order::as_str),
        })
    }
}

/// What a request changes; both absent is a read. `prefer` of
/// `Some(None)` (spelled [`FOLLOW_DEPLOYMENT`]) goes back to the
/// deployment's order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Update {
    pub offer: Option<Offer>,
    pub prefer: Option<Option<Order>>,
}

/// The spelling of `prefer` that clears a person's own order.
pub const FOLLOW_DEPLOYMENT: &str = "deployment";

impl Update {
    pub const fn is_read(self) -> bool {
        self.offer.is_none() && self.prefer.is_none()
    }
}

/// Why a request of a person's preference is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The caller is no person identified by OIDC.
    Unauthenticated,
    /// The request names another person.
    Denied,
    /// A value is not one of its spellings.
    BadRequest,
}

/// The use case's params: optional `offer` and `prefer` in their spellings,
/// and optionally the `issuer` and `subject` of the person they are for.
/// Whose row it is comes from [`target`]; nothing else is accepted: params
/// that are no object, or name any other key, are [`Refusal::BadRequest`],
/// so a misspelled field never drops silently while the rest is written.
pub const PARAMS: [&str; 4] = ["offer", "prefer", "issuer", "subject"];

/// See [`PARAMS`].
pub fn parse_update(params: &Value) -> Result<Update, Refusal> {
    let object = params.as_object().ok_or(Refusal::BadRequest)?;
    if object.keys().any(|key| !PARAMS.contains(&key.as_str())) {
        return Err(Refusal::BadRequest);
    }
    let field = |name: &str| match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(Refusal::BadRequest),
    };
    let offer = field("offer")?
        .map(|text| Offer::parse(text).ok_or(Refusal::BadRequest))
        .transpose()?;
    let prefer = field("prefer")?
        .map(|text| match text {
            FOLLOW_DEPLOYMENT => Ok(None),
            _ => Order::parse(text).map(Some).ok_or(Refusal::BadRequest),
        })
        .transpose()?;
    Ok(Update { offer, prefer })
}

/// Whose row a request reads or changes: always the caller's own OIDC
/// identity. A request that names another person (by `issuer` and
/// `subject`) is [`Refusal::Denied`]; a caller with no OIDC identity (a
/// local principal, the inbox, a planner, a job, a runner) is
/// [`Refusal::Unauthenticated`] here, though the policy refuses those roles
/// the capability first.
pub fn target(caller: Option<&OidcIdentity>, params: &Value) -> Result<OidcIdentity, Refusal> {
    let caller = caller.ok_or(Refusal::Unauthenticated)?;
    let named = |name: &str| params.get(name).and_then(Value::as_str);
    match (params.get("issuer"), params.get("subject")) {
        (None, None) => Ok(caller.clone()),
        _ if named("issuer") == Some(caller.issuer.as_str())
            && named("subject") == Some(caller.subject.as_str()) =>
        {
            Ok(caller.clone())
        }
        _ => Err(Refusal::Denied),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(subject: &str) -> OidcIdentity {
        OidcIdentity {
            issuer: "https://idp.example".into(),
            subject: subject.into(),
        }
    }

    #[test]
    fn the_deployment_follows_its_key_unless_told_and_never_allows_a_missing_one() {
        assert_eq!(
            Deployment::read(None, None, false),
            Ok(Deployment {
                team_api_key: false,
                prefer: Order::PersonalFirst
            })
        );
        assert!(Deployment::read(None, None, true).unwrap().team_api_key);
        assert!(
            !Deployment::read(Some("false"), None, true)
                .unwrap()
                .team_api_key
        );
        assert!(
            Deployment::read(Some("true"), None, true)
                .unwrap()
                .team_api_key
        );
        assert_eq!(
            Deployment::read(Some("true"), None, false),
            Err(BTreeSet::from([TEAM_API_KEY_ENV, TEAM_API_KEY_SECRET]))
        );
        assert_eq!(
            Deployment::read(Some("yes"), Some("first"), true),
            Err(BTreeSet::from([TEAM_API_KEY_ENV, PREFER_ENV]))
        );
        assert_eq!(
            Deployment::read(None, Some("team_first"), false)
                .unwrap()
                .prefer,
            Order::TeamFirst
        );
    }

    #[test]
    fn a_person_without_a_row_offers_nothing_and_an_update_replaces_only_its_fields() {
        let none = Preference::of_row(None);
        assert_eq!(none, Preference::NONE);
        assert_eq!(none.to_json(), json!({"offer": "none", "prefer": null}));
        let offered = none.updated(Update {
            offer: Some(Offer::Personal),
            prefer: None,
        });
        assert_eq!(offered.prefer, None);
        let ordered = offered.updated(Update {
            offer: None,
            prefer: Some(Some(Order::TeamFirst)),
        });
        assert_eq!(
            ordered,
            Preference {
                offer: Offer::Personal,
                prefer: Some(Order::TeamFirst)
            }
        );
        assert_eq!(ordered.updated(Update::default()), ordered);
        let followed = ordered.updated(Update {
            offer: None,
            prefer: Some(None),
        });
        assert_eq!(followed, offered);
        assert!(Update::default().is_read());
    }

    #[test]
    fn params_take_only_the_spellings() {
        assert_eq!(parse_update(&json!({})), Ok(Update::default()));
        assert_eq!(
            parse_update(
                &json!({"issuer": "https://idp.example", "subject": "me", "offer": "none"})
            ),
            Ok(Update {
                offer: Some(Offer::None),
                prefer: None
            })
        );
        assert_eq!(
            parse_update(&json!({"offer": "personal", "prefer": "team_first"})),
            Ok(Update {
                offer: Some(Offer::Personal),
                prefer: Some(Some(Order::TeamFirst))
            })
        );
        assert_eq!(
            parse_update(&json!({"prefer": "deployment"})),
            Ok(Update {
                offer: None,
                prefer: Some(None)
            })
        );
        for bad in [
            json!({"offer": "all"}),
            json!({"prefer": "personal"}),
            json!({"offer": 1}),
            json!({"offer": "personal", "prefre": "team_first"}),
            json!({"offer": "personal", "role": "admin"}),
            json!(null),
            json!(["offer", "personal"]),
            json!("personal"),
        ] {
            assert_eq!(parse_update(&bad), Err(Refusal::BadRequest), "{bad}");
        }
    }

    #[test]
    fn only_the_callers_own_row_is_reached() {
        let me = person("me");
        assert_eq!(target(Some(&me), &json!({})), Ok(me.clone()));
        assert_eq!(
            target(
                Some(&me),
                &json!({"issuer": "https://idp.example", "subject": "me"})
            ),
            Ok(me.clone())
        );
        for other in [
            json!({"issuer": "https://idp.example", "subject": "you"}),
            json!({"issuer": "https://other.example", "subject": "me"}),
            json!({"subject": "me"}),
            json!({"issuer": "https://idp.example"}),
        ] {
            assert_eq!(target(Some(&me), &other), Err(Refusal::Denied), "{other}");
        }
        assert_eq!(target(None, &json!({})), Err(Refusal::Unauthenticated));
    }
}
