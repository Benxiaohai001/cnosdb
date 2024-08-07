use std::time::Duration;

use humantime::{format_duration, parse_duration};
use serde::{Deserialize, Deserializer, Serializer};

// The signature of a serialize_with function must follow the pattern:
//
//    fn serialize<S>(&T, S) -> Result<S::Ok, S::Error>
//    where
//        S: Serializer
//
// although it may also be generic over the input types T.
pub fn serialize<S>(date: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let s = format_duration(*date).to_string();
    serializer.serialize_str(&s)
}

// The signature of a deserialize_with function must follow the pattern:
//
//    fn deserialize<'de, D>(D) -> Result<T, D::Error>
//    where
//        D: Deserializer<'de>
//
// although it may also be generic over the output types T.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use humantime::format_duration;
    use serde::{Deserialize, Serialize};

    use crate::codec::duration;

    #[test]
    fn test_format_duration() {
        assert_eq!(
            format_duration(Duration::from_nanos(1))
                .to_string()
                .as_str(),
            "1ns"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(100))
                .to_string()
                .as_str(),
            "100ns"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(1_000))
                .to_string()
                .as_str(),
            "1us"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(1_000_000))
                .to_string()
                .as_str(),
            "1ms"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(1_000_000_000))
                .to_string()
                .as_str(),
            "1s"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(10_000_000_000))
                .to_string()
                .as_str(),
            "10s"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(61_000_000_000))
                .to_string()
                .as_str(),
            "1m 1s"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(60_000_000_000))
                .to_string()
                .as_str(),
            "1m"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(3660 * 1_000_000_000))
                .to_string()
                .as_str(),
            "1h 1m"
        );
        assert_eq!(
            format_duration(Duration::from_nanos(3600 * 1_000_000_000))
                .to_string()
                .as_str(),
            "1h"
        );
    }

    #[derive(Serialize, Deserialize, Debug)]
    pub struct Foo {
        #[serde(with = "duration")]
        pub duration: Duration,
        pub name: String,
    }

    #[test]
    fn test_ok() {
        let config_str = r#"
            duration = "6h"
            name = "Bar0"
        "#;
        let foo1: Foo = toml::from_str(config_str).unwrap();
        assert_eq!(foo1.duration, Duration::from_secs(6 * 3600));
        assert_eq!(foo1.name, "Bar0".to_string());

        let config_str = r#"
            duration = "1m"
            name = "Bar1"
        "#;
        let foo1: Foo = toml::from_str(config_str).unwrap();
        assert_eq!(foo1.duration, Duration::from_secs(60));
        assert_eq!(foo1.name, "Bar1".to_string());

        let config_str = r#"
            duration = "61s"
            name = "Bar2"
        "#;
        let foo2: Foo = toml::from_str(config_str).unwrap();
        assert_eq!(foo2.duration, Duration::from_secs(61));
        assert_eq!(foo2.name, "Bar2".to_string());

        let config_str = r#"
            duration = "150ms"
            name = "Bar3"
        "#;
        let foo3: Foo = toml::from_str(config_str).unwrap();
        assert_eq!(foo3.duration, Duration::from_millis(150));
        assert_eq!(foo3.name, "Bar3".to_string());

        let config_str = r#"
            duration = "1500ns"
            name = "Bar4"
        "#;
        let foo4: Foo = toml::from_str(config_str).unwrap();
        assert_eq!(foo4.duration, Duration::from_nanos(1500));
        assert_eq!(foo4.name, "Bar4".to_string());
    }

    #[test]
    fn test_error() {
        let config_str = r#"
            duration = "a1s"
            name = "Bar1"
        "#;
        let err = toml::from_str::<Foo>(config_str).unwrap_err();
        let err_msg = format!("{}", err);
        let exp_err_msg = r#"TOML parse error at line 2, column 24
  |
2 |             duration = "a1s"
  |                        ^^^^^
expected number at 0
"#;
        assert_eq!(&err_msg, exp_err_msg);
    }
}
