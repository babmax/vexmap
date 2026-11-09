//! Validated values. Anything that reaches the database has passed through one of these.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::CoreError;

/// Who a player is. The server's UUID is the only identity (no names, no client claims).
pub type PlayerId = Uuid;

fn is_slug(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// A server region (ADR-001). Every change records where it came from.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RegionId(String);

impl RegionId {
    const GLOBAL: &str = "global";

    /// Not tied to one region: a change made from outside the game, or a flag that applies everywhere.
    pub fn global() -> Self {
        RegionId(Self::GLOBAL.to_owned())
    }

    pub fn new(value: &str) -> Result<Self, CoreError> {
        if is_slug(value, 32) {
            Ok(RegionId(value.to_owned()))
        } else {
            Err(CoreError::Invalid("region must be 1-32 characters of a-z, 0-9, - and _".to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A country's identifier. Membership decides whose positions a player sees (Phase 3).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CountryId(String);

impl CountryId {
    pub fn new(value: &str) -> Result<Self, CoreError> {
        if is_slug(value, 32) {
            Ok(CountryId(value.to_owned()))
        } else {
            Err(CoreError::Invalid("country must be 1-32 characters of a-z, 0-9, - and _".to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The name of a local mod calling the API.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClientName(String);

impl ClientName {
    pub fn new(value: &str) -> Result<Self, CoreError> {
        if is_slug(value, 32) {
            Ok(ClientName(value.to_owned()))
        } else {
            Err(CoreError::Invalid("client name must be 1-32 characters of a-z, 0-9, - and _".to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The caller's key for one logical change. Retrying with the same key does not apply the change twice.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    pub fn new(value: &str) -> Result<Self, CoreError> {
        if !value.is_empty() && value.len() <= 64 && value.bytes().all(|b| b.is_ascii_graphic()) {
            Ok(RequestId(value.to_owned()))
        } else {
            Err(CoreError::Invalid("request id must be 1-64 printable ASCII characters".to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who is asking. A player's role is always read from the database, never taken from the request (rule 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    Player(PlayerId),
    /// A local mod acting for itself. Local mods are trusted (assumption 10), and every call is journaled.
    System(ClientName),
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Actor::Player(id) => write!(f, "player:{id}"),
            Actor::System(name) => write!(f, "system:{}", name.as_str()),
        }
    }
}

/// Everything a change needs besides its own data.
#[derive(Clone, Debug)]
pub struct CommandContext {
    pub actor: Actor,
    pub region: RegionId,
    pub request_id: RequestId,
}

/// In increasing order of power, which the derived `Ord` relies on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Player,
    Trusted,
    Moderator,
    Admin,
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Player => "player",
            Role::Trusted => "trusted",
            Role::Moderator => "moderator",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, CoreError> {
        match value {
            "player" => Ok(Role::Player),
            "trusted" => Ok(Role::Trusted),
            "moderator" => Ok(Role::Moderator),
            "admin" => Ok(Role::Admin),
            "owner" => Ok(Role::Owner),
            other => Err(CoreError::Corrupt(format!("unknown role {other:?}"))),
        }
    }
}

/// Moderation areas. The discriminants are stored as bit positions, so they never change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Countries = 0,
    Places = 1,
    Wiki = 2,
    Media = 3,
    Reviews = 4,
    Ideas = 5,
}

impl Category {
    const ALL: [Category; 6] =
        [Category::Countries, Category::Places, Category::Wiki, Category::Media, Category::Reviews, Category::Ideas];

    pub(crate) fn to_bits(categories: &BTreeSet<Category>) -> i64 {
        categories.iter().fold(0, |bits, category| bits | 1 << *category as u8)
    }

    pub(crate) fn from_bits(bits: i64) -> BTreeSet<Category> {
        Self::ALL.into_iter().filter(|category| bits & (1 << *category as u8) != 0).collect()
    }
}

/// A switchable part of the map. The discriminants are the bits of `Hello.features` on the wire, so they never change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Module {
    Realtime = 0,
    Tiles = 1,
    Routing = 2,
    Countries = 3,
    Wiki = 4,
    Places = 5,
    Panoramas = 6,
    Buildings = 7,
    Moderation = 8,
    War = 9,
    Economy = 10,
    History = 11,
    Analytics = 12,
    Notifications = 13,
}

impl Module {
    pub const ALL: [Module; 14] = [
        Module::Realtime,
        Module::Tiles,
        Module::Routing,
        Module::Countries,
        Module::Wiki,
        Module::Places,
        Module::Panoramas,
        Module::Buildings,
        Module::Moderation,
        Module::War,
        Module::Economy,
        Module::History,
        Module::Analytics,
        Module::Notifications,
    ];

    pub fn bit(self) -> u64 {
        1 << self as u8
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Module::Realtime => "realtime",
            Module::Tiles => "tiles",
            Module::Routing => "routing",
            Module::Countries => "countries",
            Module::Wiki => "wiki",
            Module::Places => "places",
            Module::Panoramas => "panoramas",
            Module::Buildings => "buildings",
            Module::Moderation => "moderation",
            Module::War => "war",
            Module::Economy => "economy",
            Module::History => "history",
            Module::Analytics => "analytics",
            Module::Notifications => "notifications",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|module| module.as_str() == value)
    }
}

/// How a line of the route network is travelled. Stored by name, so the names never change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMode {
    Walk,
    Boat,
    Train,
}

impl RouteMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteMode::Walk => "walk",
            RouteMode::Boat => "boat",
            RouteMode::Train => "train",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [RouteMode::Walk, RouteMode::Boat, RouteMode::Train].into_iter().find(|mode| mode.as_str() == value)
    }
}

/// A player name as the game allows it.
pub(crate) fn validate_player_name(name: &str) -> Result<(), CoreError> {
    if !name.is_empty() && name.len() <= 16 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Ok(())
    } else {
        Err(CoreError::Invalid("player name must be 1-16 characters of A-Z, a-z, 0-9 and _".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_reject_what_would_confuse_a_log_or_a_key() {
        for bad in ["", "A", "a b", "a/b", "a\n", &"x".repeat(33)] {
            assert!(RegionId::new(bad).is_err(), "{bad:?}");
            assert!(ClientName::new(bad).is_err(), "{bad:?}");
        }
        assert!(RegionId::new("eu-west_1").is_ok());
    }

    #[test]
    fn request_ids_are_bounded_and_printable() {
        assert!(RequestId::new("").is_err());
        assert!(RequestId::new("a b").is_err());
        assert!(RequestId::new(&"x".repeat(65)).is_err());
        assert!(RequestId::new("0af7-11:x").is_ok());
    }

    #[test]
    fn category_bits_round_trip_and_ignore_unknown_bits() {
        let all: BTreeSet<_> = Category::ALL.into_iter().collect();
        assert_eq!(Category::from_bits(Category::to_bits(&all)), all);
        assert_eq!(Category::from_bits(1 << 40), BTreeSet::new());
    }

    #[test]
    fn module_bits_are_distinct_and_names_parse_back() {
        let mut seen = 0u64;
        for module in Module::ALL {
            assert_eq!(seen & module.bit(), 0);
            seen |= module.bit();
            assert_eq!(Module::parse(module.as_str()), Some(module));
        }
        assert_eq!(Module::parse("nope"), None);
    }

    #[test]
    fn roles_are_ordered_by_power() {
        assert!(Role::Player < Role::Trusted && Role::Trusted < Role::Moderator);
        assert!(Role::Moderator < Role::Admin && Role::Admin < Role::Owner);
    }
}
