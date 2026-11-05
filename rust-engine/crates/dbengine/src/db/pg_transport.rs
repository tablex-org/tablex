// How `PostgresConn` reaches the server in the native engine: plain TCP.
//
// `dbengine-postgres` shares `postgres.rs` verbatim and swaps this file for its
// TLS-capable `v2/tls.rs` with the same API (`prepare`, `connect`, `cancel`), so
// the shared source has one code path for both.

use tokio_postgres::{CancelToken, Client, Config, NoTls};

/// No TLS (`NoTls`), exactly as before.
#[derive(Clone, Default)]
pub struct Transport;

impl Transport {
    /// Parses `uri` into a configuration and the transport that serves it.
    pub fn prepare(uri: &str) -> Result<(Config, Self), tokio_postgres::Error> {
        Ok((uri.parse()?, Self))
    }

    /// Connects and drives the connection on the current runtime.
    pub async fn connect(&self, config: &Config) -> Result<Client, tokio_postgres::Error> {
        let (client, connection) = config.connect(NoTls).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("[postgres] connection error: {error}");
            }
        });
        Ok(client)
    }

    /// Delivers a cancel request for the query `token` belongs to.
    pub async fn cancel(&self, token: &CancelToken) -> Result<(), tokio_postgres::Error> {
        token.cancel_query(NoTls).await
    }
}
