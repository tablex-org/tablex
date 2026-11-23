//! The MCP server's channel to the running tablex app: `open_table` and `execute_and_show` of the
//! tool layer go through [`AppChannel`] to the socket in the app data dir (see
//! `engine_connect::app_channel`). It decides nothing; the layer's access rules ran before a
//! request gets here, and the app's own rules run when it gets there.

use engine_agent::{AgentError, AppShow};
use engine_connect::app_channel::{AppClient, ClientError, Request};

/// [`AppShow`] over the app's socket.
pub struct AppChannel {
    /// `None`: this platform has no data directory, so no socket path.
    client: Option<AppClient>,
}

impl AppChannel {
    /// The socket in the app data dir (`TABLEX_DATA_DIR` moves it).
    pub fn at_default_location() -> Self {
        Self {
            client: AppClient::at_default_location(),
        }
    }

    /// A channel to another socket (tests).
    pub fn at(client: AppClient) -> Self {
        Self {
            client: Some(client),
        }
    }

    fn send(&self, request: &Request) -> Result<(), AgentError> {
        let client = self.client.as_ref().ok_or_else(|| {
            AgentError::AppUnavailable(
                "this platform has no data directory, so there is no channel to tablex".into(),
            )
        })?;
        client.send(request).map(|_| ()).map_err(|error| match error {
            ClientError::Refused(reason) => {
                AgentError::AppRefused(format!("tablex could not do it: {reason}"))
            }
            other => AgentError::AppUnavailable(other.to_string()),
        })
    }
}

impl AppShow for AppChannel {
    fn open_object(
        &self,
        connection_id: &str,
        database: &str,
        schema: Option<&str>,
        object: &str,
    ) -> Result<(), AgentError> {
        self.send(&Request::OpenObject {
            connection_id: connection_id.to_owned(),
            database: Some(database.to_owned()),
            schema: schema.map(str::to_owned),
            object: object.to_owned(),
        })
    }

    fn show_query(
        &self,
        connection_id: &str,
        database: Option<&str>,
        text: &str,
        run: bool,
    ) -> Result<(), AgentError> {
        self.send(&Request::ShowQuery {
            connection_id: connection_id.to_owned(),
            database: database.map(str::to_owned),
            text: text.to_owned(),
            run,
        })
    }
}
