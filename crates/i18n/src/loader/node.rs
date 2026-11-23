use std::fmt;

use ahash::AHashMap;
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor, value::MapAccessDeserializer},
};

#[derive(Debug)]
pub(super) enum Node {
    Text(Box<str>),
    Section(AHashMap<Box<str>, Node>),
    Unsupported,
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(NodeVisitor)
    }
}

struct NodeVisitor;

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = Node;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a message, a section, or a value that is neither")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Node::Text(value.into()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Node::Text(value.into()))
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        AHashMap::deserialize(MapAccessDeserializer::new(map)).map(Node::Section)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}

        Ok(Node::Unsupported)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Node::Unsupported)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Node::Unsupported)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Node::Unsupported)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Node::Unsupported)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Node::Unsupported)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Node::Unsupported)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        Node::deserialize(deserializer)
    }
}
