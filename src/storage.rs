use std::{fs::File, path::Path};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use ra2a::types::{Message, StreamResponse, Task};
use serde_json::Value;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

pub struct Store {
    pool: SqlitePool,
    _lock: File,
}

impl Drop for Store {
    fn drop(&mut self) {
        // Explicitly release the open-file-description lock before closing the fd.
        // Other threads may briefly fork with the fd before their exec closes it.
        let _ = FileExt::unlock(&self._lock);
    }
}

#[derive(Debug)]
pub struct ContextRecord {
    pub native_id: Option<String>,
    pub fingerprint: String,
}

impl Store {
    pub async fn open(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("instance.lock"))?;
        lock.try_lock_exclusive()
            .context("storage directory is already used by another a2alinker instance")?;
        let options = SqliteConnectOptions::new()
            .filename(directory.join("a2alinker.db"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS contexts (
            id TEXT PRIMARY KEY, native_id TEXT, fingerprint TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY, context_id TEXT NOT NULL, state TEXT NOT NULL, updated_at TEXT NOT NULL, body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS tasks_context ON tasks(context_id);
            CREATE TABLE IF NOT EXISTS messages (
            id TEXT PRIMARY KEY, digest TEXT NOT NULL, task_id TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL, body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_task ON events(task_id, seq);")
            .execute(&pool).await?;
        Ok(Self { pool, _lock: lock })
    }

    pub async fn context(&self, id: &str) -> Result<Option<ContextRecord>> {
        Ok(
            sqlx::query("SELECT native_id, fingerprint FROM contexts WHERE id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .map(|row| ContextRecord {
                    native_id: row.get("native_id"),
                    fingerprint: row.get("fingerprint"),
                }),
        )
    }

    pub async fn set_native_id(&self, id: &str, native: &str) -> Result<()> {
        sqlx::query("UPDATE contexts SET native_id=? WHERE id=?")
            .bind(native)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn duplicate(&self, message_id: &str) -> Result<Option<(String, String)>> {
        Ok(
            sqlx::query("SELECT digest, task_id FROM messages WHERE id=?")
                .bind(message_id)
                .fetch_optional(&self.pool)
                .await?
                .map(|row| (row.get("digest"), row.get("task_id"))),
        )
    }

    pub async fn insert_task(
        &self,
        task: &Task,
        message_id: &str,
        digest: &str,
        fingerprint: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO contexts (id,fingerprint) VALUES (?,?)")
            .bind(task.context_id.as_str())
            .bind(fingerprint)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO tasks (id,context_id,state,updated_at,body) VALUES (?,?,?,?,?)")
            .bind(task.id.as_str())
            .bind(task.context_id.as_str())
            .bind(task.status.state.to_string())
            .bind(&task.status.timestamp)
            .bind(serde_json::to_string(task)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO messages (id,digest,task_id) VALUES (?,?,?)")
            .bind(message_id)
            .bind(digest)
            .bind(task.id.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO events (task_id,body) VALUES (?,?)")
            .bind(task.id.as_str())
            .bind(serde_json::to_string(&StreamResponse::Task(task.clone()))?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_reply(
        &self,
        task: &Task,
        message: &Message,
        digest: &str,
        event: &StreamResponse,
    ) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO messages (id,digest,task_id) VALUES (?,?,?)")
            .bind(message.id.as_str())
            .bind(digest)
            .bind(task.id.as_str())
            .execute(&mut *tx)
            .await?;
        let seq = save(&mut tx, task, event).await?;
        tx.commit().await?;
        Ok(seq)
    }

    pub async fn save_event(&self, task: &Task, event: &StreamResponse) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        let seq = save(&mut tx, task, event).await?;
        tx.commit().await?;
        Ok(seq)
    }

    pub async fn task(&self, id: &str) -> Result<Option<Task>> {
        let body: Option<String> = sqlx::query_scalar("SELECT body FROM tasks WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        body.map(|s| serde_json::from_str(&s).context("decode stored task"))
            .transpose()
    }

    pub async fn tasks(&self) -> Result<Vec<Task>> {
        let bodies: Vec<String> = sqlx::query_scalar("SELECT body FROM tasks ORDER BY rowid DESC")
            .fetch_all(&self.pool)
            .await?;
        bodies
            .into_iter()
            .map(|s| serde_json::from_str(&s).context("decode stored task"))
            .collect()
    }

    pub async fn active_context(&self, context_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE context_id=? AND state IN ('TASK_STATE_SUBMITTED','TASK_STATE_WORKING','TASK_STATE_INPUT_REQUIRED','TASK_STATE_AUTH_REQUIRED')")
            .bind(context_id).fetch_one(&self.pool).await?;
        Ok(n > 0)
    }

    pub async fn events(&self, task_id: &str, after: i64) -> Result<Vec<(i64, StreamResponse)>> {
        let rows = sqlx::query(
            "SELECT seq,body FROM events WHERE task_id=? AND seq>? ORDER BY seq LIMIT 128",
        )
        .bind(task_id)
        .bind(after)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let body: String = row.get("body");
                Ok((row.get("seq"), serde_json::from_str(&body)?))
            })
            .collect()
    }

    pub async fn validate_cursor(&self, task_id: &str, seq: i64) -> Result<()> {
        let belongs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE task_id=? AND seq=?")
                .bind(task_id)
                .bind(seq)
                .fetch_one(&self.pool)
                .await?;
        ensure!(belongs == 1, "Last-Event-ID does not belong to this task");
        Ok(())
    }

    pub async fn latest_event(&self, task_id: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COALESCE(MAX(seq),0) FROM events WHERE task_id=?")
                .bind(task_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn snapshot(&self, task_id: &str) -> Result<Option<(Task, i64)>> {
        let mut tx = self.pool.begin().await?;
        let body: Option<String> = sqlx::query_scalar("SELECT body FROM tasks WHERE id=?")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
        let seq: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq),0) FROM events WHERE task_id=?")
                .bind(task_id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        body.map(|body| Ok((serde_json::from_str(&body)?, seq)))
            .transpose()
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

async fn save(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    task: &Task,
    event: &StreamResponse,
) -> Result<i64> {
    sqlx::query("UPDATE tasks SET state=?,updated_at=?,body=? WHERE id=?")
        .bind(task.status.state.to_string())
        .bind(&task.status.timestamp)
        .bind(serde_json::to_string(task)?)
        .bind(task.id.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(
        sqlx::query("INSERT INTO events (task_id,body) VALUES (?,?)")
            .bind(task.id.as_str())
            .bind(serde_json::to_string(event)?)
            .execute(&mut **tx)
            .await?
            .last_insert_rowid(),
    )
}

pub fn metadata(task: &mut Task) -> &mut std::collections::HashMap<String, Value> {
    task.metadata.get_or_insert_with(Default::default)
}
