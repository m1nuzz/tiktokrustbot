use serde::{de::{self, Deserializer, Visitor}, Deserialize};
use std::fmt;

pub fn de_f64_from_string_or_number<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    struct F64Visitor;

    impl<'de> Visitor<'de> for F64Visitor {
        type Value = f64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("f64 or string")
        }

        fn visit_f64<E>(self, v: f64) -> Result<f64, E>
        where
            E: de::Error,
        {
            Ok(v)
        }

        fn visit_i64<E>(self, v: i64) -> Result<f64, E>
        where
            E: de::Error,
        {
            Ok(v as f64)
        }

        fn visit_u64<E>(self, v: u64) -> Result<f64, E>
        where
            E: de::Error,
        {
            Ok(v as f64)
        }

        fn visit_str<E>(self, v: &str) -> Result<f64, E>
        where
            E: de::Error,
        {
            let s = v.trim();
            if s.eq_ignore_ascii_case("N/A") || s.is_empty() {
                return Ok(0.0);
            }
            s.parse::<f64>().map_err(|e| E::custom(format!("invalid f64: {} ({})", v, e)))
        }

        fn visit_string<E>(self, v: String) -> Result<f64, E>
        where
            E: de::Error,
        {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(F64Visitor)
}

#[derive(Debug, Deserialize)]
pub struct Stream {
    pub width: u32,
    pub height: u32,
    #[serde(default, deserialize_with = "crate::mtproto_uploader::video_metadata::de_f64_from_string_or_number")]
    pub duration: f64,
}

#[derive(Debug, Deserialize)]
pub struct Format {
    #[serde(default, deserialize_with = "crate::mtproto_uploader::video_metadata::de_f64_from_string_or_number")]
    pub duration: f64,
}

#[derive(Debug, Deserialize)]
pub struct FFProbeOutput {
    pub streams: Vec<Stream>,
    #[serde(default)]
    pub format: Option<Format>,
}
