//! The `connections` table, as the Google token cache sees it.
//!
//! `google::TokenSource` refreshes access tokens without knowing what a
//! database is: it asks a [`ConnectionStore`] for the sealed refresh token and
//! tells it when Google refused one. This is that store, over `Db`. Opening
//! the seal is the token source's own business — it holds `GMCP_SECRET` — so
//! what leaves here is the blob exactly as it is stored.

use std::sync::Arc;

use crate::db::{ConnectionStatus, Db, DbError};
use crate::google::{BoxFuture, ConnectionStore, Error, Result};

pub struct Connections {
    db: Db,
}

impl Connections {
    pub fn new(db: Db) -> Arc<Self> {
        Arc::new(Self { db })
    }
}

impl ConnectionStore for Connections {
    fn sealed_refresh_token(&self, connection_id: i64) -> BoxFuture<'_, Result<Vec<u8>>> {
        Box::pin(async move {
            match self.db.get_connection(connection_id).await {
                Ok(c) => Ok(c.refresh_token_sealed),
                Err(DbError::NotFound) => Err(Error::Connection(format!(
                    "connection {connection_id} no longer exists"
                ))),
                Err(e) => Err(Error::Connection(format!(
                    "connection {connection_id}: {e}"
                ))),
            }
        })
    }

    /// Best effort by contract: the caller is already on its way to an error
    /// that tells the person to reconnect, and a failed write here must not
    /// replace that with a database error.
    fn mark_needs_reauth<'a>(&'a self, connection_id: i64, detail: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Err(e) = self
                .db
                .set_connection_status(connection_id, ConnectionStatus::NeedsReauth, Some(detail))
                .await
            {
                tracing::warn!("marking connection {connection_id} for re-auth: {e}");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewConnection;
    use crate::domain::seal;

    const SECRET: &[u8] = b"a development secret of thirty-two bytes or more";

    #[tokio::test]
    async fn the_store_hands_out_the_sealed_blob_and_records_a_dead_grant() {
        let db = Db::open_memory().await.unwrap();
        let user = db
            .upsert_user("s", Some("a@example.test"), None)
            .await
            .unwrap();
        let connection = db
            .create_connection(NewConnection {
                user_id: user.id,
                label: "work".into(),
                google_email: "anna@example.test".into(),
                services: vec!["gmail".into()],
                granted_scopes: vec!["openid".into()],
                refresh_token_sealed: seal::seal(SECRET, "1//refresh"),
                delegate_ok: false,
            })
            .await
            .unwrap();
        let store = Connections::new(db.clone());

        let sealed = store.sealed_refresh_token(connection.id).await.unwrap();
        assert_eq!(seal::open(SECRET, &sealed).unwrap(), "1//refresh");

        store
            .mark_needs_reauth(connection.id, "invalid_grant")
            .await;
        let after = db.get_connection(connection.id).await.unwrap();
        assert_eq!(after.status, ConnectionStatus::NeedsReauth);
        assert_eq!(after.status_detail.as_deref(), Some("invalid_grant"));

        // A connection that is gone is an error the tool can explain, not a panic.
        let missing = store.sealed_refresh_token(9999).await.unwrap_err();
        assert!(
            missing.to_string().contains("no longer exists"),
            "{missing}"
        );
    }
}
