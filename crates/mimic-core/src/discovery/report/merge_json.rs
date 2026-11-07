//! Merging a report into another while it is parsed (spec §7.3, write): the result of
//! `Report::union` of the parsed report, without ever holding the parsed report. A `Stats` is
//! read whole and merged; everything above it (scopes, template nodes, raw paths) is merged
//! into the stored tree as its members are read, so what the merge holds beside the stored
//! report is the JSON and one `Stats`.
//!
//! The JSON is what `Report::to_json` writes (the controller wrote it after the check): a key
//! that appears twice is merged twice, where `from_json` keeps the last.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{DeserializeSeed, Deserializer, Error, MapAccess, Visitor};

use super::{Direction, Node, Origin, Report, Scope, ScopeKey, Stats};

impl Report {
    /// [`Report::union`] of the report in `json`.
    pub(in crate::discovery) fn merge_json(
        &mut self,
        json: &str,
        max_literals: usize,
    ) -> Result<(), serde_json::Error> {
        let mut deserializer = serde_json::Deserializer::from_str(json);
        ReportSeed {
            into: self,
            max_literals,
        }
        .deserialize(&mut deserializer)?;
        deserializer.end()
    }
}

/// A seed that reads a JSON object with `visit_map`.
macro_rules! map_seed {
    ($seed:ident<$($lt:lifetime),*>) => {
        impl<'de> DeserializeSeed<'de> for $seed<$($lt),*> {
            type Value = ();

            fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
                deserializer.deserialize_map(self)
            }
        }

    };
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum ReportField {
    Version,
    Pod,
    Exchanges,
    OutboundExchanges,
    Scopes,
}

struct ReportSeed<'a> {
    into: &'a mut Report,
    max_literals: usize,
}

map_seed!(ReportSeed<'_>);

impl<'de> Visitor<'de> for ReportSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a report")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let (mut pod, mut exchanges, mut outbound_exchanges) = (None, None, 0u64);
        let mut scopes = false;
        while let Some(field) = map.next_key()? {
            match field {
                ReportField::Version => {
                    map.next_value::<u32>()?;
                }
                ReportField::Pod => pod = map.next_value::<Option<String>>()?,
                ReportField::Exchanges => exchanges = Some(map.next_value::<u64>()?),
                ReportField::OutboundExchanges => outbound_exchanges = map.next_value()?,
                ReportField::Scopes => {
                    scopes = true;
                    map.next_value_seed(DirectionsSeed {
                        into: &mut *self.into,
                        max_literals: self.max_literals,
                    })?;
                }
            }
        }
        let exchanges = exchanges.ok_or_else(|| A::Error::missing_field("exchanges"))?;
        if !scopes {
            return Err(A::Error::missing_field("scopes"));
        }
        // What `union` does around the scopes.
        if self.into.pod != pod {
            self.into.pod = None;
        }
        self.into.exchanges = self.into.exchanges.saturating_add(exchanges);
        self.into.outbound_exchanges = self
            .into
            .outbound_exchanges
            .saturating_add(outbound_exchanges);
        self.into.sync_version();
        Ok(())
    }
}

/// `scopes`: direction to scopes.
struct DirectionsSeed<'a> {
    into: &'a mut Report,
    max_literals: usize,
}

map_seed!(DirectionsSeed<'_>);

impl<'de> Visitor<'de> for DirectionsSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the scopes of a report")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(direction) = map.next_key::<Direction>()? {
            map.next_value_seed(ScopesSeed {
                into: self.into.scopes.entry(direction).or_default(),
                max_literals: self.max_literals,
            })?;
        }
        Ok(())
    }
}

/// The scopes of a direction.
struct ScopesSeed<'a> {
    into: &'a mut BTreeMap<ScopeKey, Scope>,
    max_literals: usize,
}

map_seed!(ScopesSeed<'_>);

impl<'de> Visitor<'de> for ScopesSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the scopes of a direction")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<ScopeKey>()? {
            map.next_value_seed(ScopeSeed {
                into: self.into.entry(key).or_default(),
                max_literals: self.max_literals,
            })?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum ScopeField {
    Root,
    Raw,
    Origin,
    Tunnels,
    LastSeen,
}

struct ScopeSeed<'a> {
    into: &'a mut Scope,
    max_literals: usize,
}

map_seed!(ScopeSeed<'_>);

impl<'de> Visitor<'de> for ScopeSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a scope")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        // What `Scope::merge` does, member by member.
        while let Some(field) = map.next_key()? {
            match field {
                ScopeField::Root => map.next_value_seed(NodeSeed {
                    into: &mut self.into.root,
                    max_literals: self.max_literals,
                })?,
                ScopeField::Raw => map.next_value_seed(RawSeed(&mut self.into.raw))?,
                ScopeField::Origin => {
                    if let Some(origin) = map.next_value::<Option<Origin>>()? {
                        self.into
                            .origin
                            .get_or_insert_with(Origin::default)
                            .merge(origin);
                    }
                }
                ScopeField::Tunnels => {
                    if let Some(tunnels) = map.next_value::<Option<BTreeMap<String, u64>>>()? {
                        let mine = self.into.tunnels.get_or_insert_with(BTreeMap::new);
                        for (kind, n) in tunnels {
                            let count = mine.entry(kind).or_default();
                            *count = count.saturating_add(n);
                        }
                    }
                }
                ScopeField::LastSeen => {
                    let day = map.next_value::<Option<u64>>()?;
                    self.into.last_seen = self.into.last_seen.max(day);
                }
            }
        }
        Ok(())
    }
}

/// `raw`: path to method to stats.
struct RawSeed<'a>(&'a mut BTreeMap<String, BTreeMap<String, Stats>>);

map_seed!(RawSeed<'_>);

impl<'de> Visitor<'de> for RawSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the raw paths of a scope")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(path) = map.next_key::<String>()? {
            map.next_value_seed(MethodsSeed(self.0.entry(path).or_default()))?;
        }
        Ok(())
    }
}

/// Method to stats: each `Stats` is read whole, then merged.
struct MethodsSeed<'a>(&'a mut BTreeMap<String, Stats>);

map_seed!(MethodsSeed<'_>);

impl<'de> Visitor<'de> for MethodsSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the endpoints of a path")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(method) = map.next_key::<String>()? {
            let stats: Stats = map.next_value()?;
            self.0.entry(method).or_default().merge(stats);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum NodeField {
    Literals,
    Param,
    Absorbing,
    Endpoints,
}

struct NodeSeed<'a> {
    into: &'a mut Node,
    max_literals: usize,
}

map_seed!(NodeSeed<'_>);

impl<'de> Visitor<'de> for NodeSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a template node")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        // What `Node::merge` does, member by member, then the absorption that closes it.
        while let Some(field) = map.next_key()? {
            match field {
                NodeField::Literals => map.next_value_seed(LiteralsSeed {
                    into: &mut self.into.literals,
                    max_literals: self.max_literals,
                })?,
                NodeField::Param => map.next_value_seed(ParamSeed {
                    into: &mut self.into.param,
                    max_literals: self.max_literals,
                })?,
                NodeField::Absorbing => self.into.absorbing |= map.next_value::<bool>()?,
                NodeField::Endpoints => {
                    map.next_value_seed(MethodsSeed(&mut self.into.endpoints))?;
                }
            }
        }
        self.into.absorb(self.max_literals);
        Ok(())
    }
}

struct LiteralsSeed<'a> {
    into: &'a mut BTreeMap<String, Node>,
    max_literals: usize,
}

map_seed!(LiteralsSeed<'_>);

impl<'de> Visitor<'de> for LiteralsSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the literal children of a template node")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(literal) = map.next_key::<String>()? {
            map.next_value_seed(NodeSeed {
                into: self.into.entry(literal).or_default(),
                max_literals: self.max_literals,
            })?;
        }
        Ok(())
    }
}

/// The parameter child: `null` leaves the stored one alone.
struct ParamSeed<'a> {
    into: &'a mut Option<Box<Node>>,
    max_literals: usize,
}

impl<'de> DeserializeSeed<'de> for ParamSeed<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for ParamSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the parameter child of a template node, or null")
    }

    fn visit_none<E: Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        NodeSeed {
            into: self.into.get_or_insert_with(Default::default),
            max_literals: self.max_literals,
        }
        .deserialize(deserializer)
    }
}
