//! CQL types as this driver needs them: read from the text of
//! `system_schema.columns.type` (`frozen<map<text, list<int>>>`) and from the
//! metadata of a result, and written back as the name a person knows.
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
use std::collections::HashMap;

/// A type without parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Native {
    Ascii,
    BigInt,
    Blob,
    Boolean,
    Counter,
    Date,
    Decimal,
    Double,
    Duration,
    Float,
    Inet,
    Int,
    SmallInt,
    Text,
    Time,
    Timestamp,
    Timeuuid,
    TinyInt,
    Uuid,
    Varint,
}

impl Native {
    const ALL: [(Native, &'static str); 20] = [
        (Native::Ascii, "ascii"),
        (Native::BigInt, "bigint"),
        (Native::Blob, "blob"),
        (Native::Boolean, "boolean"),
        (Native::Counter, "counter"),
        (Native::Date, "date"),
        (Native::Decimal, "decimal"),
        (Native::Double, "double"),
        (Native::Duration, "duration"),
        (Native::Float, "float"),
        (Native::Inet, "inet"),
        (Native::Int, "int"),
        (Native::SmallInt, "smallint"),
        (Native::Text, "text"),
        (Native::Time, "time"),
        (Native::Timestamp, "timestamp"),
        (Native::Timeuuid, "timeuuid"),
        (Native::TinyInt, "tinyint"),
        (Native::Uuid, "uuid"),
        (Native::Varint, "varint"),
    ];

    pub(super) fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(native, _)| *native == self)
            .map_or("", |(_, name)| name)
    }

    fn named(name: &str) -> Option<Native> {
        if name.eq_ignore_ascii_case("varchar") {
            return Some(Native::Text);
        }
        Self::ALL
            .iter()
            .find(|(_, known)| known.eq_ignore_ascii_case(name))
            .map(|(native, _)| *native)
    }
}

/// A CQL type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CqlType {
    Native(Native),
    List(Box<CqlType>),
    Set(Box<CqlType>),
    Map(Box<CqlType>, Box<CqlType>),
    Tuple(Vec<CqlType>),
    Vector(Box<CqlType>, u32),
    /// A user-defined type by its name; the fields are in the catalog of
    /// its keyspace ([`Udts`]).
    Udt(String),
    Frozen(Box<CqlType>),
    /// A type of a Java class, or anything this driver does not know.
    Custom(String),
}

/// The user-defined types of one keyspace: name to fields, in their order.
pub(super) type Udts = HashMap<String, Vec<(String, CqlType)>>;

impl CqlType {
    /// The type without `frozen<…>` around it.
    pub(super) fn thawed(&self) -> &CqlType {
        match self {
            CqlType::Frozen(inner) => inner.thawed(),
            other => other,
        }
    }

    pub(super) fn native(&self) -> Option<Native> {
        match self.thawed() {
            CqlType::Native(native) => Some(*native),
            _ => None,
        }
    }

    pub(super) fn is_counter(&self) -> bool {
        self.native() == Some(Native::Counter)
    }

    /// The name of the type as CQL writes it.
    pub(super) fn name(&self) -> String {
        match self {
            CqlType::Native(native) => native.name().to_owned(),
            CqlType::List(item) => format!("list<{}>", item.name()),
            CqlType::Set(item) => format!("set<{}>", item.name()),
            CqlType::Map(key, value) => format!("map<{}, {}>", key.name(), value.name()),
            CqlType::Tuple(items) => format!(
                "tuple<{}>",
                items
                    .iter()
                    .map(CqlType::name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            CqlType::Vector(item, dimensions) => format!("vector<{}, {dimensions}>", item.name()),
            CqlType::Udt(name) => name.clone(),
            CqlType::Frozen(inner) => format!("frozen<{}>", inner.name()),
            CqlType::Custom(name) => name.clone(),
        }
    }

    /// Read the text of a type as the schema tables hold it. Whatever is not
    /// understood becomes [`CqlType::Custom`] with the text as it is: such a
    /// column is shown and never written.
    pub(super) fn parse(text: &str) -> CqlType {
        let mut reader = Reader {
            text: text.as_bytes(),
            at: 0,
        };
        match reader.read() {
            Some(parsed) if reader.rest_is_empty() => parsed,
            _ => CqlType::Custom(text.trim().to_owned()),
        }
    }

    /// The type of a column of a result.
    pub(super) fn of(column: &ColumnType<'_>) -> CqlType {
        match column {
            ColumnType::Native(native) => match native {
                NativeType::Ascii => CqlType::Native(Native::Ascii),
                NativeType::BigInt => CqlType::Native(Native::BigInt),
                NativeType::Blob => CqlType::Native(Native::Blob),
                NativeType::Boolean => CqlType::Native(Native::Boolean),
                NativeType::Counter => CqlType::Native(Native::Counter),
                NativeType::Date => CqlType::Native(Native::Date),
                NativeType::Decimal => CqlType::Native(Native::Decimal),
                NativeType::Double => CqlType::Native(Native::Double),
                NativeType::Duration => CqlType::Native(Native::Duration),
                NativeType::Float => CqlType::Native(Native::Float),
                NativeType::Inet => CqlType::Native(Native::Inet),
                NativeType::Int => CqlType::Native(Native::Int),
                NativeType::SmallInt => CqlType::Native(Native::SmallInt),
                NativeType::Text => CqlType::Native(Native::Text),
                NativeType::Time => CqlType::Native(Native::Time),
                NativeType::Timestamp => CqlType::Native(Native::Timestamp),
                NativeType::Timeuuid => CqlType::Native(Native::Timeuuid),
                NativeType::TinyInt => CqlType::Native(Native::TinyInt),
                NativeType::Uuid => CqlType::Native(Native::Uuid),
                NativeType::Varint => CqlType::Native(Native::Varint),
                other => CqlType::Custom(format!("{other:?}").to_lowercase()),
            },
            ColumnType::Collection { frozen, typ } => {
                let inner = match typ {
                    CollectionType::List(item) => CqlType::List(Box::new(CqlType::of(item))),
                    CollectionType::Set(item) => CqlType::Set(Box::new(CqlType::of(item))),
                    CollectionType::Map(key, value) => {
                        CqlType::Map(Box::new(CqlType::of(key)), Box::new(CqlType::of(value)))
                    }
                    other => CqlType::Custom(format!("{other:?}").to_lowercase()),
                };
                frozen_if(*frozen, inner)
            }
            ColumnType::Vector { typ, dimensions } => {
                CqlType::Vector(Box::new(CqlType::of(typ)), u32::from(*dimensions))
            }
            ColumnType::UserDefinedType { frozen, definition } => {
                frozen_if(*frozen, CqlType::Udt(definition.name.to_string()))
            }
            ColumnType::Tuple(items) => CqlType::Tuple(items.iter().map(CqlType::of).collect()),
            other => CqlType::Custom(format!("{other:?}").to_lowercase()),
        }
    }
}

fn frozen_if(frozen: bool, inner: CqlType) -> CqlType {
    if frozen {
        CqlType::Frozen(Box::new(inner))
    } else {
        inner
    }
}

/// A reader of the text of a type.
struct Reader<'a> {
    text: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip_space(&mut self) {
        while self.text.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn rest_is_empty(&mut self) -> bool {
        self.skip_space();
        self.at == self.text.len()
    }

    fn take(&mut self, byte: u8) -> bool {
        self.skip_space();
        if self.text.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// A name: plain letters, digits and `_`, or `"…"` with `""` inside.
    fn name(&mut self) -> Option<(String, bool)> {
        self.skip_space();
        if self.text.get(self.at) == Some(&b'"') {
            let mut name = Vec::new();
            self.at += 1;
            loop {
                match self.text.get(self.at)? {
                    b'"' if self.text.get(self.at + 1) == Some(&b'"') => {
                        name.push(b'"');
                        self.at += 2;
                    }
                    b'"' => {
                        self.at += 1;
                        return Some((String::from_utf8(name).ok()?, true));
                    }
                    byte => {
                        name.push(*byte);
                        self.at += 1;
                    }
                }
            }
        }
        let start = self.at;
        while self
            .text
            .get(self.at)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte >= 0x80)
        {
            self.at += 1;
        }
        (self.at > start).then(|| {
            (
                String::from_utf8_lossy(&self.text[start..self.at]).into_owned(),
                false,
            )
        })
    }

    fn number(&mut self) -> Option<u32> {
        self.skip_space();
        let start = self.at;
        while self.text.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        std::str::from_utf8(&self.text[start..self.at])
            .ok()?
            .parse()
            .ok()
    }

    fn read(&mut self) -> Option<CqlType> {
        let (name, quoted) = self.name()?;
        if quoted {
            return Some(CqlType::Udt(name));
        }
        let lower = name.to_ascii_lowercase();
        if !self.take(b'<') {
            return Some(match Native::named(&lower) {
                Some(native) => CqlType::Native(native),
                // An unquoted name is stored in lower case.
                None => CqlType::Udt(name),
            });
        }
        let parsed = match lower.as_str() {
            "frozen" => CqlType::Frozen(Box::new(self.read()?)),
            "list" => CqlType::List(Box::new(self.read()?)),
            "set" => CqlType::Set(Box::new(self.read()?)),
            "map" => {
                let key = self.read()?;
                if !self.take(b',') {
                    return None;
                }
                CqlType::Map(Box::new(key), Box::new(self.read()?))
            }
            "tuple" => {
                let mut items = vec![self.read()?];
                while self.take(b',') {
                    items.push(self.read()?);
                }
                CqlType::Tuple(items)
            }
            "vector" => {
                let item = self.read()?;
                if !self.take(b',') {
                    return None;
                }
                CqlType::Vector(Box::new(item), self.number()?)
            }
            _ => return None,
        };
        self.take(b'>').then_some(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(native: Native) -> CqlType {
        CqlType::Native(native)
    }

    #[test]
    fn the_schema_text_of_a_type_is_read_and_written_back() {
        for text in [
            "int",
            "text",
            "timeuuid",
            "list<int>",
            "set<uuid>",
            "map<text, bigint>",
            "frozen<map<text, frozen<list<int>>>>",
            "tuple<int, text, frozen<set<inet>>>",
            "vector<float, 3>",
            "frozen<address>",
            "map<text, frozen<address>>",
        ] {
            assert_eq!(CqlType::parse(text).name(), text, "{text}");
        }
        assert_eq!(CqlType::parse("varchar"), native(Native::Text));
        assert_eq!(CqlType::parse(" INT "), native(Native::Int));
        assert_eq!(
            CqlType::parse("map<text,frozen<list<int>>>"),
            CqlType::Map(
                Box::new(native(Native::Text)),
                Box::new(CqlType::Frozen(Box::new(CqlType::List(Box::new(native(
                    Native::Int
                ))))))
            )
        );
        assert_eq!(
            CqlType::parse("vector<float, 1536>"),
            CqlType::Vector(Box::new(native(Native::Float)), 1536)
        );
        // A quoted name is a user-defined type with its letter case kept.
        assert_eq!(
            CqlType::parse("frozen<\"Adres \"\"x\"\"\">"),
            CqlType::Frozen(Box::new(CqlType::Udt("Adres \"x\"".into())))
        );
        assert_eq!(CqlType::parse("address"), CqlType::Udt("address".into()));
    }

    #[test]
    fn what_is_not_understood_is_kept_as_it_is() {
        for text in [
            "'org.apache.cassandra.db.marshal.DynamicCompositeType(a=>org.apache.cassandra.db.marshal.BytesType)'",
            "list<int",
            "map<text>",
            "vector<float, x>",
            "frozen<>",
            "",
        ] {
            assert_eq!(CqlType::parse(text), CqlType::Custom(text.into()), "{text}");
        }
    }

    #[test]
    fn frozen_is_looked_through() {
        let frozen = CqlType::parse("frozen<list<counter>>");
        assert!(matches!(frozen.thawed(), CqlType::List(_)));
        assert!(CqlType::parse("counter").is_counter());
        assert!(!CqlType::parse("bigint").is_counter());
        assert_eq!(CqlType::parse("frozen<uuid>").native(), Some(Native::Uuid));
    }

    #[test]
    fn the_type_of_a_result_column_is_named_like_the_schema_names_it() {
        let list = ColumnType::Collection {
            frozen: true,
            typ: CollectionType::List(Box::new(ColumnType::Native(NativeType::Int))),
        };
        assert_eq!(CqlType::of(&list).name(), "frozen<list<int>>");
        let map = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Map(
                Box::new(ColumnType::Native(NativeType::Text)),
                Box::new(ColumnType::Native(NativeType::Decimal)),
            ),
        };
        assert_eq!(CqlType::of(&map).name(), "map<text, decimal>");
        let vector = ColumnType::Vector {
            typ: Box::new(ColumnType::Native(NativeType::Float)),
            dimensions: 3,
        };
        assert_eq!(CqlType::of(&vector).name(), "vector<float, 3>");
        let tuple = ColumnType::Tuple(vec![
            ColumnType::Native(NativeType::Timeuuid),
            ColumnType::Native(NativeType::Duration),
        ]);
        assert_eq!(CqlType::of(&tuple).name(), "tuple<timeuuid, duration>");
    }
}
