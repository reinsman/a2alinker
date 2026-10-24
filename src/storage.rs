//! SQLite 持久化层：保存实例身份、任务、原生会话映射、消息去重记录和 SSE 事件。
//! 对外提供异步方法，SQL 参数通过 bind 传入，避免拼接客户端输入。

use std::{
    collections::BTreeSet,
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use ra2a::types::{Message, StreamResponse, Task};
use serde_json::Value;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::options::{AgentKind, IdentityOptions};

/// 持有数据库连接池和实例文件锁；只要 Store 存活，锁文件句柄就保持打开。
pub struct Store {
    pool: SqlitePool,
    _lock: File,
    directory: PathBuf,
    directory_existed: bool,
}

impl Drop for Store {
    fn drop(&mut self) {
        // 关闭句柄前显式解锁：其他线程启动子进程时，fork 可能短暂继承该句柄，
        // 直到 exec 才关闭；主动解锁可避免实例锁因此延迟释放。
        let _ = FileExt::unlock(&self._lock);
    }
}

#[derive(Debug)]
pub struct ContextRecord {
    pub native_id: Option<String>,
}

impl Store {
    pub async fn open(directory: &Path) -> Result<Self> {
        let directory_existed = directory.try_exists()?;
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
            id TEXT PRIMARY KEY, native_id TEXT, fingerprint TEXT NOT NULL DEFAULT '');
            CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY, context_id TEXT NOT NULL, state TEXT NOT NULL, updated_at TEXT NOT NULL, body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS tasks_context ON tasks(context_id);
            CREATE TABLE IF NOT EXISTS messages (
            id TEXT PRIMARY KEY, digest TEXT NOT NULL, task_id TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL, body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_task ON events(task_id, seq);
            CREATE TABLE IF NOT EXISTS instance (
            id INTEGER PRIMARY KEY CHECK(id=1), agent TEXT NOT NULL, description TEXT NOT NULL);")
            .execute(&pool).await?;
        Ok(Self {
            pool,
            _lock: lock,
            directory: directory.into(),
            directory_existed,
        })
    }

    /// 在目录锁保护下核对实例身份；不允许同一个名字静默切换 agent。
    /// 显式描述会更新本地记录，省略描述则使用原记录。
    pub(crate) async fn prepare_instance(
        &self,
        agent: AgentKind,
        identity: &mut IdentityOptions,
    ) -> Result<()> {
        let saved: Option<(String, String)> =
            sqlx::query_as("SELECT agent,description FROM instance WHERE id=1")
                .fetch_optional(&self.pool)
                .await?;
        if saved.is_some() || self.directory_existed {
            tracing::info!(event = "instance.existing", name = identity.name, storage = %self.directory.display(), "发现本地已有实例，核对 agent 类型并读取描述");
        } else {
            tracing::info!(event = "instance.new", name = identity.name, storage = %self.directory.display(), "这是新实例，将保存 agent 类型和描述");
        }
        if let Some((stored_agent, description)) = saved {
            ensure!(
                stored_agent == agent.as_str(),
                "实例 {} 的 agent 为 {stored_agent}，与本次指定的 {} 不一致，请使用原 agent 或换一个 --name",
                identity.name,
                agent.as_str()
            );
            if identity.description.is_empty() {
                identity.description = description;
            }
        } else {
            // 旧版本没有 instance 表记录；交叉检查旧任务和原生进程目录后再登记。
            let tasks = self.tasks().await?;
            let mut agents = BTreeSet::new();
            for task in &tasks {
                if let Some(kind) = task
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("a2alinker"))
                    .and_then(|metadata| metadata.get("agent"))
                    .and_then(Value::as_str)
                {
                    agents.insert(kind.to_owned());
                }
            }
            for kind in [AgentKind::Pi, AgentKind::Codex] {
                if self
                    .directory
                    .join("home")
                    .join(kind.as_str())
                    .try_exists()?
                {
                    agents.insert(kind.as_str().to_owned());
                }
            }
            ensure!(
                agents.len() <= 1,
                "实例 {} 的旧数据包含多个 agent 类型，无法确定实例身份",
                identity.name
            );
            if let Some(stored_agent) = agents.first() {
                ensure!(
                    stored_agent == agent.as_str(),
                    "实例 {} 的 agent 为 {stored_agent}，与本次指定的 {} 不一致，请使用原 agent 或换一个 --name",
                    identity.name,
                    agent.as_str()
                );
            } else {
                ensure!(
                    tasks.is_empty(),
                    "无法确定旧实例 {} 的 agent 类型，不能自动登记",
                    identity.name
                );
            }
            ensure!(
                !identity.description.trim().is_empty(),
                "实例 {} 首次登记必须提供 --desc；旧版本未保存描述，也需要补充一次",
                identity.name
            );
        }
        ensure!(
            !identity.description.trim().is_empty(),
            "实例描述不能为空，请提供 --desc"
        );
        // agent 不参与冲突更新，避免意外改写已登记的实例类型。
        sqlx::query(
            "INSERT INTO instance (id,agent,description) VALUES (1,?,?)
            ON CONFLICT(id) DO UPDATE SET description=excluded.description",
        )
        .bind(agent.as_str())
        .bind(&identity.description)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn context(&self, id: &str) -> Result<Option<ContextRecord>> {
        Ok(sqlx::query("SELECT native_id FROM contexts WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| ContextRecord {
                native_id: row.get("native_id"),
            }))
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

    /// 在同一事务中创建任务、context、去重记录和初始事件，避免只写入其中一部分。
    pub async fn insert_task(&self, task: &Task, message_id: &str, digest: &str) -> Result<()> {
        // ? 遇到错误会提前返回；事务没有 commit 就被释放时，SQLx 会安排回滚。
        let mut tx = self.pool.begin().await?;
        // 保留旧表的 NOT NULL fingerprint 列以兼容历史数据库；新任务不再计算或比较指纹。
        sqlx::query("INSERT OR IGNORE INTO contexts (id,fingerprint) VALUES (?, '')")
            .bind(task.context_id.as_str())
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

    /// 任务快照与对应事件一起提交，确保查询结果和流式事件保持一致。
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

    #[cfg(test)]
    pub async fn latest_event(&self, task_id: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COALESCE(MAX(seq),0) FROM events WHERE task_id=?")
                .bind(task_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// 在同一读取事务中获取任务及事件游标，避免两次读取之间产生事件遗漏或重复。
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

// '_ 让编译器推断事务借用的生命周期；&mut **tx 将借用解引用到 SQL 执行器。
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

/// 懒创建 metadata，并返回它的可变借用；调用者直接修改任务中原来的映射。
pub fn metadata(task: &mut Task) -> &mut std::collections::HashMap<String, Value> {
    task.metadata.get_or_insert_with(Default::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn legacy_task_identity_is_checked_before_registration_or_task_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).await.unwrap();
        let mut task = Task::new("old-task", "old-context");
        metadata(&mut task).insert("a2alinker".into(), json!({"agent":"pi"}));
        store
            .insert_task(&task, "old-message", "digest")
            .await
            .unwrap();
        let mut identity = IdentityOptions {
            name: "Alan".into(),
            description: "升级后的描述".into(),
        };
        let error = store
            .prepare_instance(AgentKind::Codex, &mut identity)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("agent 为 pi"));
        assert_eq!(
            store.task("old-task").await.unwrap().unwrap().status.state,
            task.status.state
        );
        identity.description.clear();
        assert!(
            store
                .prepare_instance(AgentKind::Pi, &mut identity)
                .await
                .unwrap_err()
                .to_string()
                .contains("--desc")
        );
        identity.description = "升级后的描述".into();
        store
            .prepare_instance(AgentKind::Pi, &mut identity)
            .await
            .unwrap();
        store.close().await;
        drop(store);
        let store = Store::open(temp.path()).await.unwrap();
        identity.description.clear();
        store
            .prepare_instance(AgentKind::Pi, &mut identity)
            .await
            .unwrap();
        assert_eq!(identity.description, "升级后的描述");
        assert!(store.task("old-task").await.unwrap().is_some());
        store.close().await;
    }

    #[tokio::test]
    async fn legacy_home_and_conflicting_or_unknown_history_are_not_silently_rebound() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).await.unwrap();
        std::fs::create_dir_all(temp.path().join("home/codex/old-profile")).unwrap();
        let mut identity = IdentityOptions::default();
        assert!(
            store
                .prepare_instance(AgentKind::Pi, &mut identity)
                .await
                .unwrap_err()
                .to_string()
                .contains("agent 为 codex")
        );
        let mut task = Task::new("old-task", "old-context");
        metadata(&mut task).insert("a2alinker".into(), json!({"agent":"pi"}));
        store
            .insert_task(&task, "old-message", "digest")
            .await
            .unwrap();
        assert!(
            store
                .prepare_instance(AgentKind::Pi, &mut identity)
                .await
                .unwrap_err()
                .to_string()
                .contains("多个 agent")
        );
        store.close().await;

        let unknown = tempfile::tempdir().unwrap();
        let store = Store::open(unknown.path()).await.unwrap();
        let task = Task::new("unknown-task", "unknown-context");
        store
            .insert_task(&task, "unknown-message", "digest")
            .await
            .unwrap();
        assert!(
            store
                .prepare_instance(AgentKind::Pi, &mut identity)
                .await
                .unwrap_err()
                .to_string()
                .contains("无法确定旧实例")
        );
        store.close().await;
    }
}
