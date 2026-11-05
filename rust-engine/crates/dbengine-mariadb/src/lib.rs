//! MariaDB driver. ABI 1 (the Swift app) without the `abi-v2` feature; ABI 2 on
//! `engine-driver-kit` with it. The v2 implementation is the one in
//! `dbengine-mysql/src/family`, compiled here with the MariaDB flavor.
//!
//! The v2 connection runs with `CLIENT_FOUND_ROWS` on: an `UPDATE` reports the rows
//! it matched, not the rows it changed.

#[cfg(not(feature = "abi-v2"))]
mod v1;
#[cfg(not(feature = "abi-v2"))]
pub use v1::*;

#[cfg(feature = "abi-v2")]
#[path = "../../dbengine-mysql/src/family/mod.rs"]
mod family;

/// The MariaDB flavor of the shared driver.
#[cfg(feature = "abi-v2")]
struct MariaDb;

#[cfg(feature = "abi-v2")]
impl family::Flavor for MariaDb {
    const CONFIG: family::FlavorConfig = family::FlavorConfig {
        driver: "mariadb",
        display_name: "MariaDB",
        uri_schemes: &["mariadb"],
        dialect: family::Dialect::MariaDb,
    };
}

#[cfg(feature = "abi-v2")]
engine_driver_kit::export_driver!(family::Backend<MariaDb>; cancel_operation, mutate, describe_object);

#[cfg(all(test, feature = "abi-v2"))]
mod v2_tests {
    use super::*;
    use engine_driver_kit::api::{Capability, DriverCapabilities};

    #[test]
    fn exported_capabilities_describe_the_mariadb_flavor() {
        let json = family::capabilities::<MariaDb>();
        let value = serde_json::to_value(&json).unwrap();
        assert_eq!(value["driver"], "mariadb");
        assert_eq!(value["display_name"], "MariaDB");
        assert_eq!(value["uri_schemes"], serde_json::json!(["mariadb"]));
        assert_eq!(value["abi_version"], 2);
        let parsed: DriverCapabilities = serde_json::from_value(value).unwrap();
        assert!(parsed.has(Capability::MetadataTableProperties));
    }
}
