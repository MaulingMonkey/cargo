//! Support for deserializing configuration via `serde`

use crate::util::config::value;
use crate::util::config::{Config, ConfigError, ConfigKey};
use crate::util::config::{ConfigValue as CV, Definition, Value};
use serde::{de, de::IntoDeserializer};
use std::collections::HashSet;
use std::vec;

/// Serde deserializer used to convert config values to a target type using
/// `Config::get`.
#[derive(Clone)]
pub(crate) struct Deserializer<'config> {
    pub(crate) config: &'config Config,
    pub(crate) key: ConfigKey,
}

macro_rules! deserialize_method {
    ($method:ident, $visit:ident, $getter:ident) => {
        fn $method<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: de::Visitor<'de>,
        {
            let v = self.config.$getter(&self.key)?.ok_or_else(||
                ConfigError::missing(&self.key))?;
            let Value{val, definition} = v;
            let res: Result<V::Value, ConfigError> = visitor.$visit(val);
            res.map_err(|e| e.with_key_context(&self.key, definition))
        }
    }
}

impl<'de, 'config> de::Deserializer<'de> for Deserializer<'config> {
    type Error = ConfigError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        // Determine if value comes from env, cli, or file, and merge env if
        // possible.
        let cv = self.config.get_cv(&self.key)?;
        let env = self.config.env.get(self.key.as_env_key());
        let env_def = Definition::Environment(self.key.as_env_key().to_string());
        let use_env = match (&cv, env) {
            (Some(cv), Some(_)) => env_def.is_higher_priority(cv.definition()),
            (None, Some(_)) => true,
            _ => false,
        };
        if use_env {
            // Future note: If you ever need to deserialize a non-self describing
            // map type, this should implement a starts_with check (similar to how
            // ConfigMapAccess does).
            let env = env.unwrap();
            let res: Result<V::Value, ConfigError> = if env == "true" || env == "false" {
                visitor.visit_bool(env.parse().unwrap())
            } else if let Ok(env) = env.parse::<i64>() {
                visitor.visit_i64(env)
            } else if self.config.cli_unstable().advanced_env
                && env.starts_with('[')
                && env.ends_with(']')
            {
                visitor.visit_seq(ConfigSeqAccess::new(self.clone())?)
            } else {
                // Try to merge if possible.
                match cv {
                    Some(CV::List(_cv_list, _cv_def)) => {
                        visitor.visit_seq(ConfigSeqAccess::new(self.clone())?)
                    }
                    _ => {
                        // Note: CV::Table merging is not implemented, as env
                        // vars do not support table values.
                        visitor.visit_str(env)
                    }
                }
            };
            return res.map_err(|e| e.with_key_context(&self.key, env_def));
        }

        if let Some(cv) = cv {
            let res: (Result<V::Value, ConfigError>, Definition) = match cv {
                CV::Integer(i, def) => (visitor.visit_i64(i), def),
                CV::String(s, def) => (visitor.visit_string(s), def),
                CV::List(_, def) => (visitor.visit_seq(ConfigSeqAccess::new(self.clone())?), def),
                CV::Table(_, def) => (
                    visitor.visit_map(ConfigMapAccess::new_map(self.clone())?),
                    def,
                ),
                CV::Boolean(b, def) => (visitor.visit_bool(b), def),
            };
            let (res, def) = res;
            return res.map_err(|e| e.with_key_context(&self.key, def));
        }
        Err(ConfigError::missing(&self.key))
    }

    deserialize_method!(deserialize_bool, visit_bool, get_bool);
    deserialize_method!(deserialize_i8, visit_i64, get_integer);
    deserialize_method!(deserialize_i16, visit_i64, get_integer);
    deserialize_method!(deserialize_i32, visit_i64, get_integer);
    deserialize_method!(deserialize_i64, visit_i64, get_integer);
    deserialize_method!(deserialize_u8, visit_i64, get_integer);
    deserialize_method!(deserialize_u16, visit_i64, get_integer);
    deserialize_method!(deserialize_u32, visit_i64, get_integer);
    deserialize_method!(deserialize_u64, visit_i64, get_integer);
    deserialize_method!(deserialize_string, visit_string, get_string_priv);

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        if self.config.has_key(&self.key) {
            visitor.visit_some(self)
        } else {
            // Treat missing values as `None`.
            visitor.visit_none()
        }
    }

    fn deserialize_struct<V>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        // Match on the magical struct name/field names that are passed in to
        // detect when we're deserializing `Value<T>`.
        //
        // See more comments in `value.rs` for the protocol used here.
        if name == value::NAME && fields == value::FIELDS {
            return visitor.visit_map(ValueDeserializer::new(self)?);
        }
        visitor.visit_map(ConfigMapAccess::new_struct(self, fields)?)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_map(ConfigMapAccess::new_map(self)?)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_seq(ConfigSeqAccess::new(self)?)
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_seq(ConfigSeqAccess::new(self)?)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_seq(ConfigSeqAccess::new(self)?)
    }

    // These aren't really supported, yet.
    serde::forward_to_deserialize_any! {
        f32 f64 char str bytes
        byte_buf unit unit_struct
        enum identifier ignored_any newtype_struct
    }
}

struct ConfigMapAccess<'config> {
    de: Deserializer<'config>,
    set_iter: <HashSet<KeyKind> as IntoIterator>::IntoIter,
    next: Option<KeyKind>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum KeyKind {
    Normal(String),
    CaseSensitive(String),
}

impl<'config> ConfigMapAccess<'config> {
    fn new_map(de: Deserializer<'config>) -> Result<ConfigMapAccess<'config>, ConfigError> {
        let mut set = HashSet::new();
        if let Some(mut v) = de.config.get_table(&de.key)? {
            // `v: Value<HashMap<String, CV>>`
            for (key, _value) in v.val.drain() {
                set.insert(KeyKind::CaseSensitive(key));
            }
        }
        if de.config.cli_unstable().advanced_env {
            // `CARGO_PROFILE_DEV_PACKAGE_`
            let env_pattern = format!("{}_", de.key.as_env_key());
            for env_key in de.config.env.keys() {
                if env_key.starts_with(&env_pattern) {
                    // `CARGO_PROFILE_DEV_PACKAGE_bar_OPT_LEVEL = 3`
                    let rest = &env_key[env_pattern.len()..];
                    // `rest = bar_OPT_LEVEL`
                    let part = rest.splitn(2, '_').next().unwrap();
                    // `part = "bar"`
                    set.insert(KeyKind::CaseSensitive(part.to_string()));
                }
            }
        }
        Ok(ConfigMapAccess {
            de,
            set_iter: set.into_iter(),
            next: None,
        })
    }

    fn new_struct(
        de: Deserializer<'config>,
        fields: &'static [&'static str],
    ) -> Result<ConfigMapAccess<'config>, ConfigError> {
        let mut set = HashSet::new();
        for field in fields {
            set.insert(KeyKind::Normal(field.to_string()));
        }

        // Assume that if we're deserializing a struct it exhaustively lists all
        // possible fields on this key that we're *supposed* to use, so take
        // this opportunity to warn about any keys that aren't recognized as
        // fields and warn about them.
        if let Some(mut v) = de.config.get_table(&de.key)? {
            for (t_key, value) in v.val.drain() {
                if set.contains(&KeyKind::Normal(t_key.to_string())) {
                    continue;
                }
                de.config.shell().warn(format!(
                    "unused config key `{}.{}` in `{}`",
                    de.key,
                    t_key,
                    value.definition()
                ))?;
            }
        }

        Ok(ConfigMapAccess {
            de,
            set_iter: set.into_iter(),
            next: None,
        })
    }
}

impl<'de, 'config> de::MapAccess<'de> for ConfigMapAccess<'config> {
    type Error = ConfigError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: de::DeserializeSeed<'de>,
    {
        match self.set_iter.next() {
            Some(key) => {
                let name = match &key {
                    KeyKind::Normal(s) | KeyKind::CaseSensitive(s) => s.as_str(),
                };
                let result = seed.deserialize(name.into_deserializer()).map(Some);
                self.next = Some(key);
                result
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        match self.next.take().expect("next field missing") {
            KeyKind::Normal(key) => self.de.key.push(&key),
            KeyKind::CaseSensitive(key) => self.de.key.push_sensitive(&key),
        }
        let result = seed.deserialize(Deserializer {
            config: self.de.config,
            key: self.de.key.clone(),
        });
        self.de.key.pop();
        result
    }
}

struct ConfigSeqAccess {
    list_iter: vec::IntoIter<(String, Definition)>,
}

impl ConfigSeqAccess {
    fn new(de: Deserializer<'_>) -> Result<ConfigSeqAccess, ConfigError> {
        let mut res = Vec::new();
        if let Some(v) = de.config._get_list(&de.key)? {
            res.extend(v.val);
        }

        // Check environment.
        if let Some(v) = de.config.env.get(de.key.as_env_key()) {
            let def = Definition::Environment(de.key.as_env_key().to_string());
            if de.config.cli_unstable().advanced_env && v.starts_with('[') && v.ends_with(']') {
                // Parse an environment string as a TOML array.
                let toml_s = format!("value={}", v);
                let toml_v: toml::Value = toml::de::from_str(&toml_s).map_err(|e| {
                    ConfigError::new(format!("could not parse TOML list: {}", e), def.clone())
                })?;
                let values = toml_v
                    .as_table()
                    .unwrap()
                    .get("value")
                    .unwrap()
                    .as_array()
                    .expect("env var was not array");
                for value in values {
                    // TODO: support other types.
                    let s = value.as_str().ok_or_else(|| {
                        ConfigError::new(
                            format!("expected string, found {}", value.type_str()),
                            def.clone(),
                        )
                    })?;
                    res.push((s.to_string(), def.clone()));
                }
            } else {
                res.extend(v.split_whitespace().map(|s| (s.to_string(), def.clone())));
            }
        }

        Ok(ConfigSeqAccess {
            list_iter: res.into_iter(),
        })
    }
}

impl<'de> de::SeqAccess<'de> for ConfigSeqAccess {
    type Error = ConfigError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        match self.list_iter.next() {
            // TODO: add `def` to error?
            Some((value, _def)) => seed.deserialize(value.into_deserializer()).map(Some),
            None => Ok(None),
        }
    }
}

/// This is a deserializer that deserializes into a `Value<T>` for
/// configuration.
///
/// This is a special deserializer because it deserializes one of its struct
/// fields into the location that this configuration value was defined in.
///
/// See more comments in `value.rs` for the protocol used here.
struct ValueDeserializer<'config> {
    hits: u32,
    definition: Definition,
    de: Deserializer<'config>,
}

impl<'config> ValueDeserializer<'config> {
    fn new(de: Deserializer<'config>) -> Result<ValueDeserializer<'config>, ConfigError> {
        // Figure out where this key is defined.
        let definition = {
            let env = de.key.as_env_key();
            let env_def = Definition::Environment(env.to_string());
            match (de.config.env.contains_key(env), de.config.get_cv(&de.key)?) {
                (true, Some(cv)) => {
                    // Both, pick highest priority.
                    if env_def.is_higher_priority(cv.definition()) {
                        env_def
                    } else {
                        cv.definition().clone()
                    }
                }
                (true, None) => env_def,
                (false, Some(cv)) => cv.definition().clone(),
                (false, None) => {
                    return Err(
                        failure::format_err!("failed to find definition of `{}`", de.key).into(),
                    )
                }
            }
        };
        Ok(ValueDeserializer {
            hits: 0,
            definition,
            de,
        })
    }
}

impl<'de, 'config> de::MapAccess<'de> for ValueDeserializer<'config> {
    type Error = ConfigError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: de::DeserializeSeed<'de>,
    {
        self.hits += 1;
        match self.hits {
            1 => seed
                .deserialize(value::VALUE_FIELD.into_deserializer())
                .map(Some),
            2 => seed
                .deserialize(value::DEFINITION_FIELD.into_deserializer())
                .map(Some),
            _ => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        // If this is the first time around we deserialize the `value` field
        // which is the actual deserializer
        if self.hits == 1 {
            return seed.deserialize(self.de.clone()).map_err(|e| {
                ConfigError::from(e).with_key_context(&self.de.key, self.definition.clone())
            });
        }

        // ... otherwise we're deserializing the `definition` field, so we need
        // to figure out where the field we just deserialized was defined at.
        match &self.definition {
            Definition::Path(path) => {
                seed.deserialize(Tuple2Deserializer(0i32, path.to_string_lossy()))
            }
            Definition::Environment(env) => {
                seed.deserialize(Tuple2Deserializer(1i32, env.as_ref()))
            }
            Definition::Cli => seed.deserialize(Tuple2Deserializer(2i32, "")),
        }
    }
}

/// A deserializer which takes two values and deserializes into a tuple of those
/// two values. This is similar to types like `StrDeserializer` in upstream
/// serde itself.
struct Tuple2Deserializer<T, U>(T, U);

impl<'de, T, U> de::Deserializer<'de> for Tuple2Deserializer<T, U>
where
    T: IntoDeserializer<'de, ConfigError>,
    U: IntoDeserializer<'de, ConfigError>,
{
    type Error = ConfigError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, ConfigError>
    where
        V: de::Visitor<'de>,
    {
        struct SeqVisitor<T, U> {
            first: Option<T>,
            second: Option<U>,
        }
        impl<'de, T, U> de::SeqAccess<'de> for SeqVisitor<T, U>
        where
            T: IntoDeserializer<'de, ConfigError>,
            U: IntoDeserializer<'de, ConfigError>,
        {
            type Error = ConfigError;
            fn next_element_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
            where
                K: de::DeserializeSeed<'de>,
            {
                if let Some(first) = self.first.take() {
                    return seed.deserialize(first.into_deserializer()).map(Some);
                }
                if let Some(second) = self.second.take() {
                    return seed.deserialize(second.into_deserializer()).map(Some);
                }
                Ok(None)
            }
        }

        visitor.visit_seq(SeqVisitor {
            first: Some(self.0),
            second: Some(self.1),
        })
    }

    serde::forward_to_deserialize_any! {
        bool u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 char str string seq
        bytes byte_buf map struct option unit newtype_struct
        ignored_any unit_struct tuple_struct tuple enum identifier
    }
}
