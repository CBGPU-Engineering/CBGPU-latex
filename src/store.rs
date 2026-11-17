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

impl Store {
    /// Rollback-only mirror, after every gateway writer has stopped. It changes
    /// only the five transient legacy tables, never Mongo projects/files/users.
    /// Consumed tickets stay consumed; new native sessions/uploads survive.
    pub async fn export_sqlite(&self, path: &Path) -> Result<usize> {
        use rusqlite::{Connection, OpenFlags, params};
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if let Ok(meta) = std::fs::symlink_metadata(path)
            && (!meta.file_type().is_file() || meta.uid() != unsafe { libc::getuid() })
        {
            return Err("Legacy state must be an owned regular file".into());
        }
        let rows =
            sqlx::query("SELECT kind,id,data,expires FROM latex_gateway.state ORDER BY kind,id")
                .fetch_all(&self.pool)
                .await
                .map_err(|_| "Cannot snapshot native transient state")?;
        let mut output = Vec::with_capacity(rows.len());
        for row in rows {
            let kind: String = row.get("kind");
            let id: String = row.get("id");
            let data: Value = row.get("data");
            let expires: f64 = row.get("expires");
            if !expires.is_finite() || expires <= 0. {
                return Err("Invalid native transient expiry".into());
            }
            output.push((kind, id, data, expires));
        }
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move||{
          let _created=std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&path).map_err(|_|"Cannot open legacy rollback state")?;
          let mut db=Connection::open_with_flags(&path,OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NOFOLLOW).map_err(|_|"Cannot open legacy rollback database")?;
          db.busy_timeout(std::time::Duration::from_secs(5)).map_err(|_|"Cannot configure rollback database")?;
          let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|_|"Legacy database still has an active writer")?;
          tx.execute_batch("CREATE TABLE IF NOT EXISTS challenges(id TEXT PRIMARY KEY,proof TEXT,expires REAL);CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,user_id TEXT,native_session TEXT,ce_cookie TEXT,expires REAL);CREATE TABLE IF NOT EXISTS tickets(id TEXT PRIMARY KEY,session_id TEXT,proof TEXT,target TEXT,expires REAL);CREATE TABLE IF NOT EXISTS uploads(id TEXT PRIMARY KEY,user_id TEXT,metadata TEXT,expires REAL,result TEXT);CREATE TABLE IF NOT EXISTS incoming(id TEXT PRIMARY KEY,user_id TEXT,name TEXT,size INTEGER,received INTEGER,expires REAL);DELETE FROM challenges;DELETE FROM sessions;DELETE FROM tickets;DELETE FROM uploads;DELETE FROM incoming;").map_err(|_|"Cannot prepare transient rollback mirror")?;
          fn string<'a>(data:&'a Value,key:&str)->Result<&'a str>{data[key].as_str().ok_or_else(||"Invalid native rollback record".into())}
          for(kind,id,data,expires)in &output{
           match kind.as_str(){
            "challenges"=>{tx.execute("INSERT INTO challenges VALUES(?1,?2,?3)",params![id,string(data,"proof")?,expires])},
            "sessions"=>{tx.execute("INSERT INTO sessions VALUES(?1,?2,?3,?4,?5)",params![id,string(data,"user_id")?,string(data,"native_session")?,string(data,"ce_cookie")?,expires])},
            "tickets"=>{tx.execute("INSERT INTO tickets VALUES(?1,?2,?3,?4,?5)",params![id,string(data,"session_id")?,string(data,"proof")?,string(data,"target")?,expires])},
            "uploads"=>{let metadata=data.get("metadata").ok_or("Invalid native upload metadata")?.to_string();let result=data.get("result").filter(|v|!v.is_null()).map(Value::to_string);tx.execute("INSERT INTO uploads(id,user_id,metadata,expires,result)VALUES(?1,?2,?3,?4,?5)",params![id,string(data,"user_id")?,metadata,expires,result])},
            "incoming"=>{tx.execute("INSERT INTO incoming VALUES(?1,?2,?3,?4,?5,?6)",params![id,string(data,"user_id")?,string(data,"name")?,data["size"].as_i64().ok_or("Invalid incoming size")?,data["received"].as_i64().ok_or("Invalid incoming offset")?,expires])},
            _=>return Err("Unknown native state kind".into())
           }.map_err(|_|"Cannot mirror native transient record")?;
          }
          tx.commit().map_err(|_|"Cannot commit transient rollback mirror")?;Ok(output.len())
        }).await.map_err(|_|"Transient rollback worker failed")?
    }
}
