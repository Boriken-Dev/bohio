//! A `[source.<name>]` table as the value a provider's schema is checked against.

#![forbid(unsafe_code)]

use bohio_api::guatiao::{List, Map, Value};

/// The table's settings as a guatiao map. A TOML date or time is carried as its text; a float that is not a number
/// (`nan`, `inf`) is the text the file spelled, which no schema accepts for a number, so the check refuses it by key.
pub fn from_table(table: &toml::Table) -> Map {
    let mut map = Map::new();
    for (key, value) in table {
        map.set(key.as_str(), from_toml(value));
    }
    map
}

fn from_toml(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::from(text.as_str()),
        toml::Value::Integer(number) => Value::from(*number),
        toml::Value::Float(number) => {
            Value::try_from(*number).unwrap_or_else(|_| Value::from(number.to_string()))
        }
        toml::Value::Boolean(flag) => Value::from(*flag),
        toml::Value::Datetime(when) => Value::from(when.to_string()),
        toml::Value::Array(items) => {
            let mut list = List::new();
            for item in items {
                list.push(from_toml(item));
            }
            Value::from(list)
        }
        toml::Value::Table(table) => Value::from(from_table(table)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_becomes_a_map_of_the_same_shape() {
        let table: toml::Table = toml::from_str(
            "path = \"/dev/ttyUSB0\"\nbaud_rate = 9600\nrts = true\nratio = 1.5\nlist = [1, 2]\n[nested]\nx = \"y\"\n",
        )
        .expect("toml");
        let map = from_table(&table);
        assert_eq!(map.len(), 6);
        assert!(map.get("nested").is_some());
        assert!(map.get("list").is_some());
    }
}
