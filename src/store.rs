use crate::Result;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::path::Path;
#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
    pub account_v2: bool,
}
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
impl Store {
    pub async fn authorized(&self, user: &str, session: &str, compute: bool) -> bool {
        let (Ok(user), Ok(session)) = (uuid::Uuid::parse_str(user), uuid::Uuid::parse_str(session))
        else {
            return false;
        };
        sqlx::query_scalar::<_, bool>("SELECT latex_gateway.authorized($1,$2,$3,$4)")
            .bind(user)
            .bind(session)
            .bind(self.account_v2)
            .bind(compute)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(false)
    }
    pub async fn get(&self, kind: &str, id: &str) -> Result<Option<Value>> {
        sqlx::query_scalar(
            "SELECT data FROM latex_gateway.state WHERE kind=$1 AND id=$2 AND expires>$3",
        )
        .bind(kind)
        .bind(id)
        .bind(now())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| "Gateway state unavailable".into())
    }
    pub async fn put(&self, kind: &str, id: &str, data: &Value, expires: f64) -> Result<()> {
        sqlx::query("INSERT INTO latex_gateway.state(kind,id,data,expires)VALUES($1,$2,$3,$4)ON CONFLICT(kind,id)DO UPDATE SET data=excluded.data,expires=excluded.expires").bind(kind).bind(id).bind(data).bind(expires).execute(&self.pool).await.map_err(|_|"Cannot persist gateway state")?;
        Ok(())
    }
    pub async fn delete(&self, kind: &str, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM latex_gateway.state WHERE kind=$1 AND id=$2")
            .bind(kind)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|_| "Cannot remove gateway state")?;
        Ok(())
    }
    pub async fn pending(&self, user: &str) -> Result<i64> {
        sqlx::query_scalar("SELECT count(*) FROM latex_gateway.state WHERE expires>$1 AND data->>'user_id'=$2 AND (kind='incoming' OR (kind='uploads' AND data->'result' IS NULL))").bind(now()).bind(user).fetch_one(&self.pool).await.map_err(|_|"Cannot count pending uploads".into())
    }
    pub async fn consume(&self, kind: &str, id: &str) -> Result<Option<Value>> {
        sqlx::query_scalar(
            "DELETE FROM latex_gateway.state WHERE kind=$1 AND id=$2 AND expires>$3 RETURNING data",
        )
        .bind(kind)
        .bind(id)
        .bind(now())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| "Cannot consume handoff".into())
    }
    pub async fn cleanup(&self, imports: &Path) -> Result<()> {
        // Remove only UUID-named expired upload artifacts; never project files.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| "Cannot expire gateway state")?;
        let rows =
            sqlx::query("SELECT kind,id FROM latex_gateway.state WHERE expires<$1 FOR UPDATE")
                .bind(now())
                .fetch_all(&mut *tx)
                .await
                .map_err(|_| "Cannot inspect expired state")?;
        for row in rows {
            let kind: String = row.get("kind");
            let id: String = row.get("id");
            if uuid::Uuid::parse_str(&id).is_ok() && matches!(kind.as_str(), "uploads" | "incoming")
            {
                let suffix = if kind == "uploads" { "zip" } else { "partial" };
                let _ = tokio::fs::remove_file(imports.join(format!("{id}.{suffix}"))).await;
            }
        }
        sqlx::query("DELETE FROM latex_gateway.state WHERE expires<$1")
            .bind(now())
            .execute(&mut *tx)
            .await
            .map_err(|_| "Cannot expire state")?;
        tx.commit().await.map_err(|_| "Cannot finish expiry")?;
        Ok(())
    }
    pub async fn import_sqlite(&self, path: &Path) -> Result<usize> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .create_if_missing(false);
        let source = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|_| "Cannot open readonly legacy gateway state")?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| "Cannot start state import")?;
        let mut count = 0;
        for kind in ["challenges", "sessions", "tickets", "uploads", "incoming"] {
            let rows = sqlx::query(&format!("SELECT * FROM {kind}"))
                .fetch_all(&source)
                .await
                .map_err(|_| "Cannot read legacy state")?;
            for row in rows {
                let id: String = row.get("id");
                let expires: f64 = row.get("expires");
                let mut data = json!({"id":id});
                for field in match kind {
                    "challenges" => vec!["proof"],
                    "sessions" => vec!["user_id", "native_session", "ce_cookie"],
                    "tickets" => vec!["session_id", "proof", "target"],
                    "uploads" => vec!["user_id", "metadata", "result"],
                    _ => vec!["user_id", "name"],
                } {
                    if let Ok(Some(v)) = row.try_get::<Option<String>, _>(field) {
                        data[field] = if field == "metadata" || field == "result" {
                            serde_json::from_str(&v).map_err(|_| "Invalid legacy state JSON")?
                        } else {
                            json!(v)
                        };
                    }
                }
                if kind == "incoming" {
                    for field in ["size", "received"] {
                        data[field] = json!(row.get::<i64, _>(field));
                    }
                }
                count+=sqlx::query("INSERT INTO latex_gateway.state(kind,id,data,expires)VALUES($1,$2,$3,$4)ON CONFLICT DO NOTHING").bind(kind).bind(id).bind(data).bind(expires).execute(&mut *tx).await.map_err(|_|"Cannot import legacy state")?.rows_affected() as usize;
            }
        }
        tx.commit()
            .await
            .map_err(|_| "Cannot commit state import")?;
        source.close().await;
        Ok(count)
    }
}
