//! MySQL driver. ABI 1 (the Swift app) without the `abi-v2` feature; ABI 2 on
//! `engine-driver-kit` with it. The v2 implementation is shared with MariaDB:
//! see [`family`].
//!
//! The v2 connection runs with `CLIENT_FOUND_ROWS` on: an `UPDATE` reports the rows
//! it matched, not the rows it changed (details in `family`'s module docs).

#[cfg(not(feature = "abi-v2"))]
mod v1;
#[cfg(not(feature = "abi-v2"))]
pub use v1::*;

#[cfg(feature = "abi-v2")]
mod family;

/// The MySQL flavor of the shared driver.
#[cfg(feature = "abi-v2")]
struct MySql;

#[cfg(feature = "abi-v2")]
impl family::Flavor for MySql {
    const CONFIG: family::FlavorConfig = family::FlavorConfig {
        driver: "mysql",
        display_name: "MySQL",
        uri_schemes: &["mysql"],
        dialect: family::Dialect::MySql,
    };
}

#[cfg(feature = "abi-v2")]
engine_driver_kit::export_driver!(family::Backend<MySql>; cancel_operation, mutate, describe_object);

#[cfg(all(test, feature = "abi-v2"))]
mod v2_tests {
    use super::*;
    use engine_driver_kit::api::{Capability, DriverCapabilities};

    #[test]
    fn exported_capabilities_describe_the_mysql_flavor() {
        let json = family::capabilities::<MySql>();
        let value = serde_json::to_value(&json).unwrap();
        assert_eq!(value["driver"], "mysql");
        assert_eq!(value["display_name"], "MySQL");
        assert_eq!(value["uri_schemes"], serde_json::json!(["mysql"]));
        assert_eq!(value["abi_version"], 2);
        let parsed: DriverCapabilities = serde_json::from_value(value).unwrap();
        assert!(parsed.has(Capability::MetadataTableProperties));
    }
}
