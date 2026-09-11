pub(super) mod optional_str {
    use std::fmt::Display;
    use std::str::FromStr;

    use serde::{self, Deserialize, Deserializer, Serializer};

    pub(in crate::config) fn serialize<S, T>(
        value: &Option<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Display,
    {
        match value {
            Some(v) => serializer.serialize_str(&v.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub(in crate::config) fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        opt.map(|s| T::from_str(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}
