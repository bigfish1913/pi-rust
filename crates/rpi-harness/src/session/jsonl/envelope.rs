//! Reserve only the outer JSONL envelope; nested values use serde_json unchanged.
use serde::de::{MapAccess, Visitor};
use serde::Deserializer;
use serde_json::{Map, Value};

struct Envelope;

impl<'de> Visitor<'de> for Envelope {
    type Value = Map<String, Value>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let Some(first_key) = access.next_key::<String>()? else {
            return Ok(Map::new());
        };
        // Value recognizes private protocol keys specially. Do not emulate that
        // protocol here, including escaped keys or future unified serde features.
        if first_key.starts_with("$serde_json::private::") {
            return Err(serde::de::Error::custom("use canonical Value decoder"));
        }
        // Ordinary envelopes have roughly 8–12 fields. Bound the reservation to
        // this one object, not every small/nested user-provided JSON object.
        let mut map = Map::with_capacity(12);
        map.insert(first_key, access.next_value::<Value>()?);
        while let Some((key, value)) = access.next_entry::<String, Value>()? {
            map.insert(key, value);
        }
        Ok(map)
    }
}

/// An optimization attempt, never the authority for JSON/schema errors. The
/// caller must retry any failure through the original Value decoder on the same
/// in-memory line (not by reopening storage).
pub(super) fn parse(line: &str) -> serde_json::Result<Map<String, Value>> {
    let mut deserializer = serde_json::Deserializer::from_str(line);
    let map = deserializer.deserialize_map(Envelope)?;
    deserializer.end()?;
    Ok(map)
}
