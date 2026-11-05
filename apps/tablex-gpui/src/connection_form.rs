use crate::connection_tls::{TlsFamily, TlsSettings};
#[cfg(test)]
use crate::connection_tls::TlsMode;
use crate::connections::{self, ConnectionKind, SavedConnection};
use crate::settings::connection::{ConnectionProfile, OverridesExt as _};
use crate::ssh_tunnel::{SshSecretInput, SshSettings};
use engine_storage::connection::SavedConnectionRecord;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionFormState {
    pub id: Option<Uuid>,
    pub name: String,
    pub kind: ConnectionKind,
    pub uri: String,
    pub password: String,
    pub save_password: bool,
    pub is_pinned: bool,
    pub host: String,
    pub port: String,
    pub username: String,
    pub database: String,
    pub auth_database: String,
    pub uri_options: String,
    pub file_path: String,
    pub use_uri: bool,
    /// Sidebar color (`""` = none). `None` keeps the stored/default color.
    pub color_hex: Option<String>,
    /// Connect timeout in seconds (Advanced tab); lives in the URL params
    /// (`connect_timeout` / `connectTimeoutMS`).
    pub connect_timeout: String,
    /// ClickHouse: HTTPS (`secure=true`). Redis: TLS (`rediss://`).
    pub secure: bool,
    /// ClickHouse only: PEM/DER file of the trusted CA (`ca_cert=`).
    pub ca_cert: String,
    /// PostgreSQL / MySQL / MariaDB / MongoDB: the TLS tab. Lives in the URL params
    /// (`sslmode`, `sslrootcert`, ...: see `connection_tls`); a key passphrase has no field because
    /// no driver supports one.
    pub tls: TlsSettings,
    /// Connection type and setting overrides. `None` keeps what the stored
    /// record has (a form built without looking at the record must not wipe
    /// them); the dialog always sets it.
    pub profile: Option<ConnectionProfile>,
    /// The SSH tunnel. `None` keeps what the stored record has (a form built without looking at
    /// the record must not wipe it); the dialog always sets it.
    pub ssh: Option<SshSettings>,
    /// The SSH password / key passphrase typed in the dialog (empty = untouched).
    pub ssh_secrets: SshSecretInput,
}

impl Default for ConnectionFormState {
    fn default() -> Self {
        Self {
            id: None,
            name: String::new(),
            kind: ConnectionKind::Postgres,
            uri: String::new(),
            password: String::new(),
            save_password: true,
            is_pinned: false,
            host: String::new(),
            port: String::new(),
            username: String::new(),
            database: String::new(),
            auth_database: String::new(),
            uri_options: String::new(),
            file_path: String::new(),
            use_uri: true,
            color_hex: None,
            connect_timeout: String::new(),
            secure: false,
            ca_cert: String::new(),
            tls: TlsSettings::default(),
            profile: None,
            ssh: None,
            ssh_secrets: SshSecretInput::default(),
        }
    }
}

impl ConnectionFormState {
    pub fn new(kind: ConnectionKind) -> Self {
        Self {
            kind,
            use_uri: false,
            ..Self::default()
        }
    }

    /// The ClickHouse HTTP port field after the Secure toggle flips: a blank
    /// or default port follows the toggle (8123 plain, 8443 TLS); a custom
    /// port is kept (`None`).
    pub fn clickhouse_port_for_secure(port: &str, secure: bool) -> Option<&'static str> {
        const PLAIN: &str = "8123";
        const TLS: &str = "8443";
        let port = port.trim();
        let follows = port.is_empty() || port == PLAIN || port == TLS;
        follows.then_some(if secure { TLS } else { PLAIN })
    }

    pub fn from_saved(saved: &SavedConnection) -> Self {
        let mut form = Self {
            id: Some(saved.id),
            name: saved.name.clone(),
            kind: saved.kind,
            uri: saved.uri.clone(),
            password: String::new(),
            save_password: saved.save_password,
            is_pinned: saved.is_pinned,
            ..Self::default()
        };
        form.parse_uri_fields();
        form
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.kind.is_supported() {
            return Err("This database type is not supported by the current shell".to_string());
        }
        if self.name.trim().is_empty() {
            return Err("Connection name is required".to_string());
        }
        if let Some(profile) = &self.profile {
            profile.overrides.validate()?;
        }
        let uri = self.effective_uri()?;
        if uri.is_empty() {
            return Err("Connection URI is required".to_string());
        }
        connections::validate_uri(self.kind, &uri)?;
        TlsSettings::reject_passphrase(&uri).map_err(|error| error.to_string())?;
        if let Some((family, tls, _)) = self.tls_in_fields() {
            tls.check(family)
                .and_then(|()| tls.check_files(family))
                .map_err(|error| error.to_string())?;
        }
        if let Some(ssh) = self.ssh.as_ref().filter(|ssh| ssh.routed()) {
            ssh.validate().map_err(|error| error.to_string())?;
            // A connection that cannot go through a tunnel is refused where it is saved.
            crate::ssh_tunnel::plan_for(self.kind, &uri, ssh)
                .map_err(|error| error.explain(ssh.plan_route()))?;
        }
        Ok(())
    }

    pub fn validate_recovery(&self) -> Result<(), String> {
        self.validate()?;
        let has_password = !self.password.trim().is_empty()
            || self
                .password_from_uri()
                .is_some_and(|password| !password.trim().is_empty());
        if self.kind != ConnectionKind::Sqlite && !has_password {
            return Err("Connection password is required".to_string());
        }
        Ok(())
    }

    pub fn persisted_uri(&self) -> Result<String, String> {
        self.validate()?;
        connections::sanitized_uri(&self.effective_uri()?)
    }

    pub fn runtime_uri(&self) -> Result<String, String> {
        self.validate()?;
        let uri = self.effective_uri()?;
        if self.kind == ConnectionKind::Sqlite || self.password.is_empty() {
            return Ok(connections::with_default_postgres_database(uri));
        }
        connections::runtime_uri(&uri, &self.password)
            .map(connections::with_default_postgres_database)
    }

    #[cfg(test)]
    pub fn generated_uri(&self) -> Result<String, String> {
        self.effective_uri()
    }

    #[allow(clippy::wrong_self_convention)] // fills the password from the URI while it builds the record
    pub fn to_record(
        &mut self,
        now: f64,
        original: Option<&SavedConnectionRecord>,
    ) -> Result<SavedConnectionRecord, String> {
        if self.password.is_empty() {
            if let Some(password) = self.password_from_uri() {
                self.password = password;
            }
        }
        let id = match self.id {
            Some(id) => id,
            None => {
                let id = Uuid::new_v4();
                self.id = Some(id);
                id
            }
        };
        let mut record = original.cloned().unwrap_or_else(|| SavedConnectionRecord {
            id: id.to_string().to_uppercase(),
            name: String::new(),
            uri: String::new(),
            db_type: String::new(),
            color_hex: "#34C759".to_string(),
            save_password: true,
            save_proxy_password: false,
            network_settings_json: "{}".to_string(),
            is_pinned: false,
            last_connected: None,
            created_at: now,
            updated_at: now,
            connection_type: String::new(),
            overrides_json: String::new(),
        });
        record.id = id.to_string().to_uppercase();
        record.name = self.name.trim().to_string();
        record.uri = self.persisted_uri()?;
        record.db_type = match self.kind {
            ConnectionKind::Postgres => connections::DB_TYPE_POSTGRESQL.to_string(),
            ConnectionKind::Sqlite => connections::DB_TYPE_SQLITE.to_string(),
            ConnectionKind::Mongodb => connections::DB_TYPE_MONGODB.to_string(),
            ConnectionKind::Mysql => connections::DB_TYPE_MYSQL.to_string(),
            ConnectionKind::Mariadb => connections::DB_TYPE_MARIADB.to_string(),
            ConnectionKind::Redis => connections::DB_TYPE_REDIS.to_string(),
            ConnectionKind::Clickhouse => connections::DB_TYPE_CLICKHOUSE.to_string(),
        };
        let reuses_saved_password = self.password.is_empty()
            && original.is_some_and(|original| {
                original.save_password
                    && connections::persisted_uris_match(self.kind, &original.uri, &record.uri)
            });
        if let Some(color) = &self.color_hex {
            record.color_hex = color.clone();
        }
        record.save_password =
            self.save_password && (!self.password.is_empty() || reuses_saved_password);
        record.is_pinned = self.is_pinned;
        if let Some(profile) = &self.profile {
            record.connection_type = profile.type_id.clone();
            record.overrides_json = profile.overrides.to_json();
        }
        if let Some(ssh) = &self.ssh {
            // A connection that never had a tunnel keeps its settings untouched.
            let has_section = record.network_settings_json.contains("\"ssh\"");
            // Settings that cannot be read are rewritten, so a damaged record can be repaired by
            // saving it (the dialog says so).
            let unreadable = SshSettings::parse(&record.network_settings_json).is_err();
            if ssh.routed() || has_section || unreadable || *ssh != SshSettings::default() {
                record.network_settings_json = ssh.write_into(&record.network_settings_json);
            }
        }
        record.updated_at = now;
        Ok(record)
    }

    pub fn parse_uri_fields(&mut self) {
        if self.kind == ConnectionKind::Sqlite {
            let uri = self.uri.trim();
            if uri
                .get(..9)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("sqlite://"))
            {
                self.file_path = uri[9..].to_string();
                self.use_uri = false;
            }
            return;
        }

        let Ok(url) = url::Url::parse(&self.uri) else {
            return;
        };
        let Some(host) = url.host_str() else {
            return;
        };
        if self.kind == ConnectionKind::Mongodb
            && (url.scheme() == "mongodb+srv" || host.contains(','))
        {
            return;
        }
        self.host = host.to_string();
        // `https://host` means 443 to the ClickHouse driver, like any URL.
        let port = match self.kind {
            ConnectionKind::Clickhouse => url.port_or_known_default(),
            _ => url.port(),
        };
        self.port = port.map(|port| port.to_string()).unwrap_or_default();
        self.username = decode_uri_component(url.username());
        self.password = url.password().map(decode_uri_component).unwrap_or_default();
        self.database = decode_uri_component(url.path().trim_start_matches('/'));
        self.uri_options = url.query().unwrap_or_default().to_string();
        self.extract_connect_timeout();
        self.tls = TlsSettings::default();
        if let Some(family) = TlsFamily::of(self.kind) {
            let (tls, rest) = TlsSettings::extract(family, &self.uri_options);
            self.tls = tls;
            self.uri_options = rest;
        }
        if self.kind == ConnectionKind::Redis {
            self.secure = url.scheme() == "rediss";
        }
        if self.kind == ConnectionKind::Clickhouse {
            self.extract_clickhouse_options(&url);
        }
        if self.kind == ConnectionKind::Mongodb {
            if let Some((_, value)) = url.query_pairs().find(|(key, _)| key == "authSource") {
                self.auth_database = value.into_owned();
                self.uri_options = self
                    .uri_options
                    .split('&')
                    .filter(|part| !part.starts_with("authSource="))
                    .collect::<Vec<_>>()
                    .join("&");
            }
        }
        self.use_uri = false;
    }

    /// `connect_timeout=N` (PostgreSQL) / `connectTimeoutMS=N` (MongoDB) for
    /// the Advanced-tab timeout, when set and valid.
    fn connect_timeout_param(&self) -> Option<String> {
        let seconds = self.connect_timeout.trim().parse::<u32>().ok().filter(|s| *s > 0)?;
        match self.kind {
            ConnectionKind::Postgres => Some(format!("connect_timeout={seconds}")),
            ConnectionKind::Mongodb => Some(format!("connectTimeoutMS={}", seconds * 1000)),
            _ => None,
        }
    }

    /// The TLS settings the fields produce and the free-text options without TLS parameters; `None`
    /// for a kind without a TLS tab or a URL used as entered. A TLS parameter typed into the
    /// options fills what the tab leaves blank, so none is ever written twice.
    fn tls_in_fields(&self) -> Option<(TlsFamily, TlsSettings, String)> {
        let family = TlsFamily::of(self.kind).filter(|_| !self.use_uri)?;
        let (typed, rest) = TlsSettings::extract(family, self.uri_options.trim());
        Some((family, self.tls.clone().or(typed), rest))
    }

    /// Moves `secure` / `ssl` / `ca_cert` out of `uri_options` into their own
    /// fields; the rest stays as the extra settings.
    fn extract_clickhouse_options(&mut self, url: &url::Url) {
        self.secure = url.scheme() == "https";
        self.ca_cert.clear();
        let mut settings = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "secure" | "ssl" => {
                    self.secure = matches!(
                        value.to_ascii_lowercase().as_str(),
                        "true" | "1" | "yes" | "on"
                    );
                }
                "ca_cert" => self.ca_cert = value.into_owned(),
                _ => {
                    settings.append_pair(&key, &value);
                }
            }
        }
        self.uri_options = settings.finish();
    }

    /// Moves the timeout param out of `uri_options` into `connect_timeout`.
    fn extract_connect_timeout(&mut self) {
        let mut kept = Vec::new();
        for part in self.uri_options.split('&').filter(|part| !part.is_empty()) {
            match (self.kind, part.split_once('=')) {
                (ConnectionKind::Postgres, Some(("connect_timeout", value))) => {
                    self.connect_timeout = value.to_string();
                }
                (ConnectionKind::Mongodb, Some(("connectTimeoutMS", value))) => {
                    if let Ok(ms) = value.parse::<u32>() {
                        self.connect_timeout = (ms / 1000).max(1).to_string();
                    } else {
                        kept.push(part);
                    }
                }
                _ => kept.push(part),
            }
        }
        self.uri_options = kept.join("&");
    }

    fn effective_uri(&self) -> Result<String, String> {
        if self.use_uri {
            let uri = self.uri.trim();
            return Ok(connections::normalize_uri_scheme(uri));
        }
        if self.kind == ConnectionKind::Sqlite {
            let uri = self.uri.trim();
            let path = if self.file_path.trim().is_empty() {
                uri.get(..9)
                    .filter(|scheme| scheme.eq_ignore_ascii_case("sqlite://"))
                    .map(|_| &uri[9..])
                    .unwrap_or_default()
            } else {
                self.file_path.trim()
            };
            if path.is_empty() {
                return Ok(String::new());
            }
            return Ok(format!("sqlite://{path}"));
        }
        let host = self.host.trim();
        if host.is_empty() {
            return Ok(connections::normalize_uri_scheme(self.uri.trim()));
        }
        let scheme = match self.kind {
            ConnectionKind::Postgres => "postgresql",
            ConnectionKind::Redis if self.secure => "rediss",
            _ => self.kind.expected_scheme(),
        };
        let mut url = url::Url::parse(&format!("{scheme}://{host}"))
            .map_err(|_| "Invalid connection URI".to_string())?;
        if !self.username.trim().is_empty() {
            url.set_username(self.username.trim())
                .map_err(|_| "Invalid connection URI".to_string())?;
        }
        if !self.password.is_empty() {
            url.set_password(Some(&self.password))
                .map_err(|_| "Invalid connection URI".to_string())?;
        }
        if !self.port.trim().is_empty() {
            let port = self
                .port
                .trim()
                .parse::<u16>()
                .map_err(|_| "Port must be between 1 and 65535".to_string())?;
            if port == 0 {
                return Err("Port must be between 1 and 65535".to_string());
            }
            url.set_port(Some(port))
                .map_err(|_| "Invalid connection URI".to_string())?;
        }
        if !self.database.trim().is_empty() {
            url.set_path(&format!("/{}", self.database.trim()));
        }
        let tls = self.tls_in_fields();
        let mut options = match &tls {
            Some((_, _, rest)) => rest.clone(),
            None => self.uri_options.trim().to_string(),
        };
        if let Some(param) = self.connect_timeout_param() {
            if !options.is_empty() {
                options.push('&');
            }
            options.push_str(&param);
        }
        if let Some((family, tls, _)) = &tls {
            tls.append_to(*family, &mut options);
        }
        if !options.is_empty() {
            url.set_query(Some(&options));
        }
        if self.kind == ConnectionKind::Mongodb && !self.auth_database.trim().is_empty() {
            url.query_pairs_mut()
                .append_pair("authSource", self.auth_database.trim());
        }
        let ca_cert = self.ca_cert.trim();
        if self.kind == ConnectionKind::Clickhouse && (self.secure || !ca_cert.is_empty()) {
            let mut pairs = url.query_pairs_mut();
            if self.secure {
                pairs.append_pair("secure", "true");
            }
            if !ca_cert.is_empty() {
                pairs.append_pair("ca_cert", ca_cert);
            }
        }
        Ok(url.to_string())
    }

    pub(crate) fn password_from_uri(&self) -> Option<String> {
        let uri = self.effective_uri().ok()?;
        if self.kind == ConnectionKind::Mongodb {
            let authority_start = uri.find("://")? + 3;
            let authority_end = uri[authority_start..]
                .find(['/', '?', '#'])
                .map(|offset| authority_start + offset)
                .unwrap_or(uri.len());
            let authority = &uri[authority_start..authority_end];
            let userinfo = authority.rsplit_once('@')?.0;
            let password = userinfo.split_once(':')?.1;
            return Some(decode_uri_component(password));
        }
        url::Url::parse(&uri)
            .ok()?
            .password()
            .map(decode_uri_component)
    }
}

pub(crate) fn decode_uri_component(value: &str) -> String {
    fn hex_value(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(kind: ConnectionKind, uri: &str) -> ConnectionFormState {
        ConnectionFormState {
            name: "Local database".to_string(),
            kind,
            uri: uri.to_string(),
            password: "p@ss:word".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn postgres_form_sanitizes_persisted_uri_and_rehydrates_runtime_uri() {
        let form = form(
            ConnectionKind::Postgres,
            "postgresql://alice:old-secret@localhost:5432/app",
        );

        assert_eq!(
            form.persisted_uri().unwrap(),
            "postgresql://alice@localhost:5432/app"
        );
        assert_eq!(
            form.runtime_uri().unwrap(),
            "postgresql://alice:p%40ss%3Aword@localhost:5432/app"
        );
    }

    #[test]
    fn sqlite_form_accepts_sqlite_uri_without_password() {
        let form = form(ConnectionKind::Sqlite, "sqlite:///tmp/tablex.db");

        assert_eq!(form.persisted_uri().unwrap(), "sqlite:///tmp/tablex.db");
        assert_eq!(form.runtime_uri().unwrap(), "sqlite:///tmp/tablex.db");
    }

    #[test]
    fn sqlite_uri_scheme_is_parsed_case_insensitively() {
        let mut form = form(ConnectionKind::Sqlite, "SQLITE:///tmp/tablex.db");

        form.parse_uri_fields();

        assert_eq!(form.file_path, "/tmp/tablex.db");
        assert!(!form.use_uri);
    }

    #[test]
    fn sqlite_url_mode_normalizes_the_scheme_for_runtime() {
        let form = form(ConnectionKind::Sqlite, "SQLITE:///tmp/tablex.db");

        assert_eq!(form.runtime_uri().unwrap(), "sqlite:///tmp/tablex.db");
    }

    #[test]
    fn sqlite_fields_fallback_normalizes_an_uppercase_uri_scheme() {
        let form = ConnectionFormState {
            name: "SQLite fallback".to_string(),
            kind: ConnectionKind::Sqlite,
            uri: "SQLITE:///tmp/tablex.db".to_string(),
            use_uri: false,
            ..Default::default()
        };

        assert_eq!(form.runtime_uri().unwrap(), "sqlite:///tmp/tablex.db");
    }

    #[test]
    fn mongodb_url_mode_normalizes_an_uppercase_uri_scheme() {
        let form = ConnectionFormState {
            name: "Mongo uppercase".to_string(),
            kind: ConnectionKind::Mongodb,
            uri: "MONGODB://mongo.example.test/app".to_string(),
            use_uri: true,
            ..Default::default()
        };

        assert_eq!(
            form.runtime_uri().unwrap(),
            "mongodb://mongo.example.test/app"
        );
    }

    #[test]
    fn url_mode_password_is_available_for_record_persistence() {
        let mut form = ConnectionFormState {
            name: "Mongo cluster".to_string(),
            kind: ConnectionKind::Mongodb,
            uri: "mongodb+srv://alice:p%40ss%3Aword@cluster.example.com/app".to_string(),
            save_password: true,
            use_uri: true,
            ..Default::default()
        };

        let record = form.to_record(42.0, None).unwrap();

        assert_eq!(form.password, "p@ss:word");
        assert!(record.save_password);
        assert_eq!(record.uri, "mongodb+srv://alice@cluster.example.com/app");
    }

    #[test]
    fn fields_mode_uri_fallback_persists_embedded_password() {
        let mut form = ConnectionFormState {
            name: "Postgres fallback".to_string(),
            kind: ConnectionKind::Postgres,
            uri: "postgresql://alice:p%40ss%3Aword@localhost/app".to_string(),
            use_uri: false,
            save_password: true,
            ..Default::default()
        };

        let record = form.to_record(42.0, None).unwrap();

        assert_eq!(form.password, "p@ss:word");
        assert!(record.save_password);
        assert_eq!(record.uri, "postgresql://alice@localhost/app");
    }

    #[test]
    fn recovery_validation_accepts_password_embedded_in_uri() {
        let form = ConnectionFormState {
            name: "Postgres recovery".to_string(),
            kind: ConnectionKind::Postgres,
            uri: "postgresql://alice:p%40ss@localhost/app".to_string(),
            use_uri: true,
            ..Default::default()
        };

        assert!(form.validate_recovery().is_ok());
    }

    #[test]
    fn mongodb_auth_source_is_encoded_when_fields_build_a_uri() {
        let mut form = ConnectionFormState::new(ConnectionKind::Mongodb);
        form.name = "Mongo auth source".to_string();
        form.host = "mongo.example.test".to_string();
        form.auth_database = "admin&ops".to_string();

        assert_eq!(
            form.persisted_uri().unwrap(),
            "mongodb://mongo.example.test?authSource=admin%26ops"
        );
    }

    #[test]
    fn mongodb_auth_source_round_trips_reserved_characters() {
        let mut form = ConnectionFormState {
            name: "Mongo auth source".to_string(),
            kind: ConnectionKind::Mongodb,
            uri: "mongodb://alice@mongo.example.test/app?authSource=admin%26ops%2B%3D".to_string(),
            use_uri: true,
            ..Default::default()
        };

        form.parse_uri_fields();

        assert_eq!(form.auth_database, "admin&ops+=");
        assert_eq!(
            form.persisted_uri().unwrap(),
            "mongodb://alice@mongo.example.test/app?authSource=admin%26ops%2B%3D"
        );
    }

    #[test]
    fn form_rejects_wrong_scheme_and_missing_name() {
        let mut form = form(ConnectionKind::Sqlite, "postgresql://localhost/app");
        assert!(form.validate().is_err());

        form.kind = ConnectionKind::Postgres;
        form.uri = "postgresql://localhost/app".to_string();
        form.name.clear();
        assert_eq!(form.validate().unwrap_err(), "Connection name is required");
    }

    #[test]
    fn record_uses_swift_compatible_uppercase_uuid_and_no_password() {
        let mut form = form(ConnectionKind::Postgres, "postgresql://localhost/app");
        form.id = Some(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap());
        let record = form.to_record(42.0, None).unwrap();

        assert_eq!(record.id, "11111111-1111-1111-1111-111111111111");
        assert!(!record.uri.contains("p@ss"));
        assert_eq!(record.created_at, 42.0);
    }

    #[test]
    fn passwordless_new_record_does_not_claim_a_keychain_secret() {
        let mut form = form(ConnectionKind::Postgres, "postgresql://localhost/app");
        form.password.clear();

        let record = form.to_record(42.0, None).unwrap();

        assert!(!record.save_password);
    }

    #[test]
    fn new_record_assigns_generated_uuid_back_to_form() {
        let mut form = form(ConnectionKind::Postgres, "postgresql://localhost/app");
        assert!(form.id.is_none());
        let record = form.to_record(42.0, None).unwrap();

        assert_eq!(
            form.id.map(|id| id.to_string()),
            Some(record.id.to_lowercase())
        );
    }

    #[test]
    fn recovery_validation_requires_a_new_password() {
        let mut form = form(ConnectionKind::Postgres, "postgresql://localhost/app");
        form.password.clear();

        assert_eq!(
            form.validate_recovery().unwrap_err(),
            "Connection password is required"
        );
    }

    #[test]
    fn redis_form_builds_redis_and_rediss_uris_with_acl_user_and_db_index() {
        let mut form = ConnectionFormState::new(ConnectionKind::Redis);
        form.name = "cache".into();
        form.host = "cache.example.com".into();
        form.port = "6380".into();
        form.username = "app".into();
        form.password = "s3cret".into();
        form.database = "3".into();
        assert_eq!(
            form.generated_uri().unwrap(),
            "redis://app:s3cret@cache.example.com:6380/3"
        );
        // The persisted URI never carries the password.
        assert_eq!(form.persisted_uri().unwrap(), "redis://app@cache.example.com:6380/3");
        form.secure = true;
        assert_eq!(
            form.runtime_uri().unwrap(),
            "rediss://app:s3cret@cache.example.com:6380/3"
        );
        // And the same URI parses back into the fields, TLS included.
        let mut parsed = ConnectionFormState::new(ConnectionKind::Redis);
        parsed.uri = "rediss://app@cache.example.com:6380/3".into();
        parsed.parse_uri_fields();
        assert!(parsed.secure && !parsed.use_uri);
        assert_eq!(
            (parsed.host.as_str(), parsed.port.as_str(), parsed.username.as_str(), parsed.database.as_str()),
            ("cache.example.com", "6380", "app", "3")
        );
        parsed.secure = false;
        assert_eq!(parsed.generated_uri().unwrap(), "redis://app@cache.example.com:6380/3");
        // A password-only (no ACL user) server.
        let mut legacy = ConnectionFormState::new(ConnectionKind::Redis);
        legacy.name = "legacy".into();
        legacy.host = "localhost".into();
        legacy.password = "pw".into();
        assert_eq!(legacy.runtime_uri().unwrap(), "redis://:pw@localhost");
        assert!(legacy.validate().is_ok());
    }

    #[test]
    fn connect_timeout_round_trips_through_url_params() {
        let mut form = form(ConnectionKind::Postgres, "postgresql://u@h:5432/db?sslmode=prefer&connect_timeout=7");
        form.parse_uri_fields();
        assert_eq!(form.connect_timeout, "7");
        // `sslmode` is the TLS tab's, not free text.
        assert_eq!(form.uri_options, "");
        assert_eq!(form.tls.mode, TlsMode::Prefer);
        form.connect_timeout = "12".into();
        let uri = form.generated_uri().unwrap();
        assert!(uri.contains("sslmode=prefer") && uri.contains("connect_timeout=12"), "{uri}");
        form.connect_timeout.clear();
        assert!(!form.generated_uri().unwrap().contains("connect_timeout"));
        let mut mongo = form_for_mongo();
        mongo.parse_uri_fields();
        assert_eq!(mongo.connect_timeout, "3");
        assert!(mongo.generated_uri().unwrap().contains("connectTimeoutMS=3000"));
    }

    fn form_for_mongo() -> ConnectionFormState {
        form(ConnectionKind::Mongodb, "mongodb://h:27017/db?connectTimeoutMS=3000")
    }

    #[test]
    fn color_is_written_only_when_chosen() {
        let mut chosen = form(ConnectionKind::Postgres, "postgresql://u@h/db");
        chosen.name = "x".into();
        chosen.color_hex = Some("#ef4444".into());
        assert_eq!(chosen.to_record(1.0, None).unwrap().color_hex, "#ef4444");
        chosen.color_hex = Some(String::new());
        assert_eq!(chosen.to_record(1.0, None).unwrap().color_hex, "", "none swatch");
        chosen.color_hex = None;
        assert_eq!(chosen.to_record(1.0, None).unwrap().color_hex, "#34C759");
    }

    #[test]
    fn postgres_fields_build_a_connection_uri() {
        let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
        form.name = "Local database".to_string();
        form.host = "db.example.test".to_string();
        form.port = "5433".to_string();
        form.username = "alice".to_string();
        form.database = "app".to_string();
        form.uri_options = "sslmode=disable".to_string();

        assert_eq!(
            form.persisted_uri().unwrap(),
            "postgresql://alice@db.example.test:5433/app?sslmode=disable"
        );
    }

    #[test]
    fn postgres_without_a_database_connects_to_the_postgres_maintenance_db() {
        let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
        form.name = "No database".to_string();
        form.host = "10.0.0.5".to_string();
        form.port = "5000".to_string();
        form.username = "app-read-only-user".to_string();

        // Stored URI keeps the database optional; only the runtime URI defaults it.
        assert_eq!(
            form.persisted_uri().unwrap(),
            "postgresql://app-read-only-user@10.0.0.5:5000"
        );
        form.password = "s3cret".to_string();
        assert_eq!(
            form.runtime_uri().unwrap(),
            "postgresql://app-read-only-user:s3cret@10.0.0.5:5000/postgres"
        );
        assert_eq!(
            connections::with_default_postgres_database(
                "postgresql://u@h:5432/app".to_string()
            ),
            "postgresql://u@h:5432/app"
        );
        assert_eq!(
            connections::with_default_postgres_database(
                "postgres://u@h:5432?dbname=app".to_string()
            ),
            "postgres://u@h:5432?dbname=app"
        );
        assert_eq!(
            connections::with_default_postgres_database("mongodb://h/".to_string()),
            "mongodb://h/"
        );
    }

    #[test]
    fn uri_fields_decode_credentials_and_database_path() {
        let mut form = ConnectionFormState {
            name: "Encoded database".to_string(),
            kind: ConnectionKind::Postgres,
            uri: "postgresql://alice%40example:p%40ss%3Aword@localhost/app%20db".to_string(),
            ..Default::default()
        };

        form.parse_uri_fields();

        assert_eq!(form.username, "alice@example");
        assert_eq!(form.password, "p@ss:word");
        assert_eq!(form.database, "app db");
        assert_eq!(
            form.persisted_uri().unwrap(),
            "postgresql://alice%40example@localhost/app%20db"
        );
    }

    #[test]
    fn editing_a_uri_without_an_explicit_port_keeps_it_unchanged() {
        let saved = SavedConnection {
            id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            name: "Local database".to_string(),
            uri: "postgresql://alice@localhost/app".to_string(),
            kind: ConnectionKind::Postgres,
            save_password: true,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        };

        let form = ConnectionFormState::from_saved(&saved);

        assert!(form.port.is_empty());
        assert_eq!(form.persisted_uri().unwrap(), saved.uri);
    }

    #[test]
    fn editing_postgres_scheme_alias_reuses_saved_password() {
        let saved = SavedConnection {
            id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            name: "Local database".to_string(),
            uri: "postgres://alice@localhost/app".to_string(),
            kind: ConnectionKind::Postgres,
            save_password: true,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        };
        let mut form = ConnectionFormState::from_saved(&saved);
        let original = SavedConnectionRecord {
            id: saved.id.to_string(),
            name: saved.name.clone(),
            uri: saved.uri.clone(),
            db_type: connections::DB_TYPE_POSTGRESQL.to_string(),
            color_hex: "#34C759".to_string(),
            save_password: true,
            save_proxy_password: false,
            network_settings_json: "{}".to_string(),
            is_pinned: false,
            last_connected: None,
            created_at: 1.0,
            updated_at: 1.0,
            connection_type: String::new(),
            overrides_json: String::new(),
        };

        let record = form.to_record(2.0, Some(&original)).unwrap();

        assert_eq!(record.uri, "postgresql://alice@localhost/app");
        assert!(record.save_password);
    }

    #[test]
    fn mongodb_fields_preserve_auth_source_and_runtime_password() {
        let mut form = ConnectionFormState::new(ConnectionKind::Mongodb);
        form.name = "Local MongoDB".to_string();
        form.host = "mongo.example.test".to_string();
        form.port = "27018".to_string();
        form.username = "alice".to_string();
        form.password = "p@ss:word".to_string();
        form.database = "app".to_string();
        form.auth_database = "admin".to_string();

        assert_eq!(
            form.persisted_uri().unwrap(),
            "mongodb://alice@mongo.example.test:27018/app?authSource=admin"
        );
        assert_eq!(
            form.runtime_uri().unwrap(),
            "mongodb://alice:p%40ss%3Aword@mongo.example.test:27018/app?authSource=admin"
        );
    }

    #[test]
    fn edit_record_preserves_storage_metadata() {
        let mut form = form(ConnectionKind::Postgres, "postgresql://localhost/app");
        form.id = Some(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap());
        form.name = "Renamed".to_string();
        let original = SavedConnectionRecord {
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            name: "Original".to_string(),
            uri: "postgresql://localhost/old".to_string(),
            db_type: connections::DB_TYPE_POSTGRESQL.to_string(),
            color_hex: "#123456".to_string(),
            save_password: true,
            save_proxy_password: true,
            network_settings_json: "{\"ssh\":{}}".to_string(),
            is_pinned: true,
            last_connected: Some(99.0),
            created_at: 12.0,
            updated_at: 88.0,
            connection_type: String::new(),
            overrides_json: String::new(),
        };

        let record = form.to_record(100.0, Some(&original)).unwrap();

        assert_eq!(record.name, "Renamed");
        assert_eq!(record.color_hex, original.color_hex);
        assert_eq!(record.save_proxy_password, original.save_proxy_password);
        assert_eq!(record.network_settings_json, original.network_settings_json);
        assert_eq!(record.last_connected, original.last_connected);
        assert_eq!(record.created_at, original.created_at);
        assert_eq!(record.updated_at, 100.0);
    }

    #[test]
    fn the_connection_type_and_overrides_are_saved_in_the_record() {
        use crate::settings::connection::{ConnectionProfile, OverrideValue};
        let mut form = form(ConnectionKind::Postgres, "postgresql://localhost/app");
        let record = form.to_record(1.0, None).unwrap();
        assert_eq!((record.connection_type.as_str(), record.overrides_json.as_str()), ("", ""));

        let mut profile = ConnectionProfile { type_id: "production".into(), ..Default::default() };
        profile.overrides.set("query.query_timeout_secs", OverrideValue::Number(30.0));
        form.profile = Some(profile.clone());
        let saved = form.to_record(2.0, Some(&record)).unwrap();
        assert_eq!(saved.connection_type, "production");
        assert_eq!(ConnectionProfile::from_record(&saved), profile, "the record reads back as saved");

        // A form that never looked at the profile keeps what the record has.
        form.profile = None;
        let kept = form.to_record(3.0, Some(&saved)).unwrap();
        assert_eq!(ConnectionProfile::from_record(&kept), profile);
        // "Use global" and "no type" clear them.
        form.profile = Some(ConnectionProfile::default());
        let cleared = form.to_record(4.0, Some(&saved)).unwrap();
        assert_eq!((cleared.connection_type.as_str(), cleared.overrides_json.as_str()), ("", ""));
    }

    #[test]
    fn an_invalid_override_blocks_saving() {
        use crate::settings::connection::{ConnectionProfile, OverrideValue};
        let mut form = form(ConnectionKind::Postgres, "postgresql://localhost/app");
        for (id, value) in [
            ("results.page_size", OverrideValue::Number(0.0)),
            ("query.query_timeout_secs", OverrideValue::Number(f64::NAN)),
            ("editor.confirm_dangerous", OverrideValue::Number(1.0)),
        ] {
            let mut profile = ConnectionProfile::default();
            profile.overrides.set(id, value);
            form.profile = Some(profile);
            assert!(form.validate().is_err(), "{id}");
            assert!(form.to_record(1.0, None).is_err(), "{id}");
        }
        let mut profile = ConnectionProfile::default();
        profile.overrides.set("results.page_size", OverrideValue::Number(25.0));
        form.profile = Some(profile);
        assert!(form.validate().is_ok());
    }

    fn clickhouse_form() -> ConnectionFormState {
        ConnectionFormState {
            name: "Analytics".to_string(),
            kind: ConnectionKind::Clickhouse,
            use_uri: false,
            host: "db.example.com".to_string(),
            port: "8123".to_string(),
            username: "al ice".to_string(),
            password: "p@ss:w/rd#".to_string(),
            database: "web".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn clickhouse_fields_build_an_encoded_uri_without_the_password_in_storage() {
        let form = clickhouse_form();

        assert_eq!(
            form.persisted_uri().unwrap(),
            "clickhouse://al%20ice@db.example.com:8123/web"
        );
        assert_eq!(
            form.runtime_uri().unwrap(),
            "clickhouse://al%20ice:p%40ss%3Aw%2Frd%23@db.example.com:8123/web"
        );
    }

    #[test]
    fn clickhouse_secure_ca_cert_and_settings_become_query_pairs() {
        let mut form = clickhouse_form();
        form.port = "8443".to_string();
        form.secure = true;
        form.ca_cert = " /etc/my ca.pem ".to_string();
        form.uri_options = "max_execution_time=30&log_comment=a%20b".to_string();

        let uri = form.persisted_uri().unwrap();
        let url = url::Url::parse(&uri).unwrap();
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("max_execution_time".to_string(), "30".to_string()),
                ("log_comment".to_string(), "a b".to_string()),
                ("secure".to_string(), "true".to_string()),
                ("ca_cert".to_string(), "/etc/my ca.pem".to_string()),
            ]
        );
        assert_eq!(url.port(), Some(8443));
        // Plain HTTP adds neither.
        let mut plain = clickhouse_form();
        plain.uri_options = "async_insert=1".to_string();
        assert!(plain.persisted_uri().unwrap().ends_with("/web?async_insert=1"));
    }

    #[test]
    fn clickhouse_uri_round_trips_through_the_fields() {
        let mut form = clickhouse_form();
        form.port = "8443".to_string();
        form.secure = true;
        form.ca_cert = "~/certs/ca.pem".to_string();
        form.uri_options = "max_execution_time=30".to_string();
        let uri = form.runtime_uri().unwrap();

        let mut parsed = ConnectionFormState::new(ConnectionKind::Clickhouse);
        parsed.uri = uri;
        parsed.parse_uri_fields();

        assert!(!parsed.use_uri);
        assert_eq!(
            (parsed.host.as_str(), parsed.port.as_str(), parsed.username.as_str()),
            ("db.example.com", "8443", "al ice")
        );
        assert_eq!(parsed.password, "p@ss:w/rd#");
        assert_eq!(parsed.database, "web");
        assert!(parsed.secure);
        assert_eq!(parsed.ca_cert, "~/certs/ca.pem");
        assert_eq!(parsed.uri_options, "max_execution_time=30");
    }

    #[test]
    fn clickhouse_accepts_http_urls_and_reads_their_own_default_ports() {
        let mut https = ConnectionFormState::new(ConnectionKind::Clickhouse);
        https.uri = "https://ch.example.com/db".to_string();
        https.parse_uri_fields();
        assert!(https.secure);
        assert_eq!(https.port, "443", "https://host means port 443 to the driver");
        assert!(connections::validate_uri(ConnectionKind::Clickhouse, "https://ch.example.com/db").is_ok());
        assert!(connections::validate_uri(ConnectionKind::Clickhouse, "http://ch.example.com").is_ok());
        assert!(connections::validate_uri(ConnectionKind::Clickhouse, "postgres://h/db").is_err());

        let mut legacy = ConnectionFormState::new(ConnectionKind::Clickhouse);
        legacy.uri = "clickhouse://h/db?ssl=1".to_string();
        legacy.parse_uri_fields();
        assert!(legacy.secure && legacy.port.is_empty() && legacy.uri_options.is_empty());
    }

    #[test]
    fn the_secure_toggle_moves_only_a_default_or_blank_port() {
        let port = ConnectionFormState::clickhouse_port_for_secure;
        assert_eq!(port("8123", true), Some("8443"));
        assert_eq!(port("8443", false), Some("8123"));
        assert_eq!(port("", true), Some("8443"));
        assert_eq!(port(" ", false), Some("8123"));
        assert_eq!(port("9440", true), None);
        assert_eq!(port("9440", false), None);
    }

    #[test]
    fn clickhouse_connection_round_trips_through_storage() {
        let mut form = clickhouse_form();
        form.secure = true;
        let record = form.to_record(10.0, None).unwrap();
        assert_eq!(record.db_type, "clickhouse");
        assert!(!record.uri.contains("p%40ss"), "no password in storage: {}", record.uri);

        let saved = SavedConnection::from_record(&record);
        assert_eq!(saved.kind, ConnectionKind::Clickhouse);
        assert!(!saved.disabled);
        assert_eq!(ConnectionKind::from_persisted("clickhouse"), Some(ConnectionKind::Clickhouse));
        assert_eq!(saved.kind.driver_id(), "clickhouse");
        assert_eq!(saved.kind.expected_scheme(), "clickhouse");
        assert!(saved.kind.is_supported());

        let reopened = ConnectionFormState::from_saved(&saved);
        assert_eq!(reopened.kind, ConnectionKind::Clickhouse);
        assert!(reopened.secure);
        assert_eq!(reopened.host, "db.example.com");
        assert_eq!(reopened.username, "al ice");
        // Re-saving the reopened form keeps the same persisted URI.
        assert_eq!(reopened.persisted_uri().unwrap(), record.uri);
    }

    #[test]
    fn a_shared_clickhouse_url_keeps_tls_routing_but_drops_settings() {
        let uri = "clickhouse://alice:secret@h:8443/db?secure=true&ca_cert=%2Fetc%2Fca.pem&log_comment=token";
        assert_eq!(
            connections::public_connection_uri(uri).unwrap(),
            "clickhouse://alice@h:8443/db?secure=true&ca_cert=%2Fetc%2Fca.pem"
        );
        assert!(!ConnectionKind::Clickhouse.applies_batches_atomically());
        assert!(ConnectionKind::Postgres.applies_batches_atomically());
    }

    /// A real file the TLS checks can find; removed with the guard.
    struct TempPem(std::path::PathBuf);

    impl TempPem {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("tablex-tls-{}-{name}", std::process::id()));
            std::fs::write(&path, "-----BEGIN CERTIFICATE-----\n").unwrap();
            Self(path)
        }

        fn path(&self) -> String {
            self.0.display().to_string()
        }
    }

    impl Drop for TempPem {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn encoded(path: &str) -> String {
        percent_encoding::utf8_percent_encode(path, crate::connection_tls::VALUE).to_string()
    }

    #[test]
    fn postgres_tls_round_trips_through_the_saved_uri_and_the_runtime_uri() {
        let (ca, cert, key) = (TempPem::new("pg-ca.pem"), TempPem::new("pg-cert.pem"), TempPem::new("pg-key.pem"));
        let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
        form.name = "tls".into();
        form.host = "db.example.test".into();
        form.username = "alice".into();
        form.password = "s3cret".into();
        form.tls = TlsSettings {
            mode: TlsMode::VerifyFull,
            ca_file: ca.path(),
            client_cert: cert.path(),
            client_key: key.path(),
            ..Default::default()
        };
        let record = form.to_record(1.0, None).unwrap();
        let query = format!(
            "?sslmode=verify-full&sslrootcert={}&sslcert={}&sslkey={}",
            encoded(&ca.path()),
            encoded(&cert.path()),
            encoded(&key.path())
        );
        assert_eq!(record.uri, format!("postgresql://alice@db.example.test{query}"));
        assert!(!record.uri.contains("s3cret"));
        // The driver gets the same parameters, with the password from the credential store.
        assert_eq!(
            form.runtime_uri().unwrap(),
            format!("postgresql://alice:s3cret@db.example.test/postgres{query}")
        );
        // Editing keeps them: the reopened form shows them and writes the same URI back.
        let reopened = ConnectionFormState::from_saved(&SavedConnection::from_record(&record));
        assert_eq!(reopened.tls, form.tls);
        assert_eq!(reopened.uri_options, "");
        assert_eq!(reopened.persisted_uri().unwrap(), record.uri);
    }

    #[test]
    fn mysql_and_mariadb_tls_use_the_mysql_parameter_names() {
        let (ca, cert, key) = (TempPem::new("my-ca.pem"), TempPem::new("my-cert.pem"), TempPem::new("my-key.pem"));
        for kind in [ConnectionKind::Mysql, ConnectionKind::Mariadb] {
            let mut form = ConnectionFormState::new(kind);
            form.name = "tls".into();
            form.host = "db.example.test".into();
            form.tls = TlsSettings {
                mode: TlsMode::VerifyFull,
                ca_file: ca.path(),
                client_cert: cert.path(),
                client_key: key.path(),
                server_name: "db.internal".into(),
            };
            let uri = form.persisted_uri().unwrap();
            let query = uri.split_once('?').unwrap().1;
            assert!(query.starts_with("ssl-mode=VERIFY_IDENTITY&ssl-ca="), "{query}");
            assert!(query.ends_with("&tls-server-name=db.internal"), "{query}");
            let mut reopened = ConnectionFormState::new(kind);
            reopened.uri = uri;
            reopened.parse_uri_fields();
            assert_eq!(reopened.tls, form.tls);
        }
    }

    #[test]
    fn mongodb_tls_writes_tls_and_the_certificate_files() {
        let (ca, pem) = (TempPem::new("mongo-ca.pem"), TempPem::new("mongo-client.pem"));
        let mut form = ConnectionFormState::new(ConnectionKind::Mongodb);
        form.name = "tls".into();
        form.host = "mongo.example.test".into();
        form.tls = TlsSettings {
            mode: TlsMode::Require,
            ca_file: ca.path(),
            client_cert: pem.path(),
            // Ignored: MongoDB takes the key from the certificate file.
            client_key: "/ignored.pem".into(),
            ..Default::default()
        };
        let uri = form.persisted_uri().unwrap();
        assert!(uri.contains("tls=true&tlsAllowInvalidCertificates=true&tlsCAFile="), "{uri}");
        assert!(uri.contains("&tlsCertificateKeyFile="), "{uri}");
        assert!(!uri.contains("ignored"));
        let mut reopened = ConnectionFormState::new(ConnectionKind::Mongodb);
        reopened.uri = uri;
        reopened.parse_uri_fields();
        assert_eq!(reopened.tls.mode, TlsMode::Require);
        assert_eq!((reopened.tls.ca_file, reopened.tls.client_cert), (ca.path(), pem.path()));
    }

    #[test]
    fn a_tls_option_typed_into_the_url_params_is_not_written_twice() {
        let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
        form.name = "tls".into();
        form.host = "h".into();
        form.uri_options = "sslmode=require&application_name=x".into();
        // The tab is blank: the typed value is used, once.
        assert_eq!(form.persisted_uri().unwrap(), "postgresql://h?application_name=x&sslmode=require");
        // The tab wins over the typed value.
        form.tls.mode = TlsMode::Disable;
        assert_eq!(form.persisted_uri().unwrap(), "postgresql://h?application_name=x&sslmode=disable");
    }

    #[test]
    fn invalid_tls_combinations_are_refused_with_the_reason() {
        let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
        form.name = "tls".into();
        form.host = "h".into();
        form.tls.mode = TlsMode::VerifyCa;
        assert!(form.validate().unwrap_err().contains("verify-ca needs a CA certificate file"));
        assert!(form.to_record(1.0, None).is_err());
        form.tls.mode = TlsMode::VerifyFull;
        form.tls.client_cert = "/c.pem".into();
        assert!(form.validate().unwrap_err().contains("together"));
        form.tls.client_cert.clear();
        form.tls.ca_file = std::env::temp_dir().join("tablex-no-such-ca.pem").display().to_string();
        let error = form.validate().unwrap_err();
        assert!(error.contains("does not exist") && error.contains("CA certificate"), "{error}");
        form.tls.ca_file = "system".into();
        assert!(form.validate().is_ok());
        form.kind = ConnectionKind::Mysql;
        form.tls.mode = TlsMode::VerifyCa;
        form.tls.ca_file.clear();
        assert!(form.validate().unwrap_err().contains("VERIFY_CA needs a CA certificate file"));
    }

    #[test]
    fn a_key_passphrase_never_reaches_the_stored_uri() {
        let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
        form.name = "tls".into();
        form.host = "h".into();
        form.uri_options = "sslpassword=hunter2".into();
        let error = form.to_record(1.0, None).unwrap_err();
        assert!(error.contains("sslpassword") && !error.contains("hunter2"), "{error}");
        let mut mongo = ConnectionFormState {
            name: "m".into(),
            kind: ConnectionKind::Mongodb,
            uri: "mongodb+srv://u@c.example.com/db?tlsCertificateKeyFilePassword=x".into(),
            use_uri: true,
            ..Default::default()
        };
        assert!(mongo.to_record(1.0, None).is_err());
    }

    #[test]
    fn clickhouse_and_redis_keep_their_own_tls_switches() {
        for kind in [ConnectionKind::Clickhouse, ConnectionKind::Redis, ConnectionKind::Sqlite] {
            assert_eq!(TlsFamily::of(kind), None);
        }
        let mut form = ConnectionFormState::new(ConnectionKind::Clickhouse);
        form.name = "ch".into();
        form.host = "h".into();
        form.uri_options = "sslmode=require".into();
        // Free text of a kind without a TLS tab is untouched.
        assert_eq!(form.persisted_uri().unwrap(), "clickhouse://h?sslmode=require");
    }
}
