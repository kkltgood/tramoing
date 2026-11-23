//! 历史流量落盘（SQLite）。
//!
//! # 为什么按分钟存
//!
//! 按小时 rollup 会让「最近一小时」只剩一个点，做不出曲线；
//! 按秒存又太占地方。折中是**按分钟**：一天 1440 行 × 有流量的应用，
//! 三十天也就几 MB，而且 1 小时 / 24 小时 / 7 天三种视图都能从同一张表算出来。
//!
//! # 为什么只存增量
//!
//! 每 5 秒把这段时间内每个应用的字节增量累加进对应分钟的行。
//! 存增量而不是累计值，查询时直接求和，不用处理「进程重启导致累计归零」。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::functions::{Aggregate, Context, FunctionFlags};
use rusqlite::{params, Connection, OpenFlags};
use tm_core::AppKey;

pub mod files;
mod migration;

/// 保留多少天的历史
pub const RETENTION_DAYS: i64 = 30;

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "数据库错误：{e}"),
            Self::Io(e) => write!(f, "文件错误：{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// 某个应用在一段时间内的合计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppTotal {
    pub app: String,
    pub rx: u64,
    pub tx: u64,
}

impl AppTotal {
    pub fn total(&self) -> u64 {
        self.rx.saturating_add(self.tx)
    }
}

/// 曲线上的一点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub ts: i64,
    pub rx: u64,
    pub tx: u64,
}

pub struct Store {
    conn: Connection,
}

/// SQLite 的 SUM 在整数溢出时会中止整个查询；统计值沿用存储层的饱和规则。
struct SaturatingSum;

impl Aggregate<i64, i64> for SaturatingSum {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<i64> {
        Ok(0)
    }

    fn step(&self, ctx: &mut Context<'_>, sum: &mut i64) -> rusqlite::Result<()> {
        *sum = sum.saturating_add(ctx.get::<i64>(0)?.max(0));
        Ok(())
    }

    fn finalize(&self, _: &mut Context<'_>, sum: Option<i64>) -> rusqlite::Result<i64> {
        Ok(sum.unwrap_or_default())
    }
}

impl Store {
    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        conn.create_aggregate_function(
            "tm_sum",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            SaturatingSum,
        )?;
        Ok(Self { conn })
    }

    /// 查询/导出已有历史；不建目录、不迁移，也不申请写锁。
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::from_connection(conn)
    }

    pub fn open(path: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(files::parent_dir(path))?;
        let conn = Connection::open(path)?;
        let store = Self::from_connection(conn)?;
        store.migrate()?;
        // WAL 让「后台写入」和「界面查询」不会互相卡住
        let _ = store.conn.pragma_update(None, "journal_mode", "WAL");
        let _ = store.conn.pragma_update(None, "synchronous", "NORMAL");
        Ok(store)
    }

    /// 只用于测试。
    pub fn open_memory() -> Result<Self, StoreError> {
        let store = Self::from_connection(Connection::open_in_memory()?)?;
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS samples (
                minute INTEGER NOT NULL,
                app    TEXT    NOT NULL,
                rx     INTEGER NOT NULL,
                tx     INTEGER NOT NULL,
                PRIMARY KEY (minute, app)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS meta (
                k TEXT PRIMARY KEY,
                v TEXT NOT NULL
            );
            "#,
        )?;
        self.conn.execute(
            "INSERT INTO meta (k, v) VALUES ('schema', '1') ON CONFLICT(k) DO NOTHING",
            [],
        )?;
        Ok(())
    }

    /// 写入一批增量。同一分钟内的多次写入会累加。
    /// 返回实际写入（或更新）的行数。
    pub fn add_deltas(
        &mut self,
        at: SystemTime,
        deltas: &[(AppKey, u64, u64)],
    ) -> Result<usize, StoreError> {
        self.add_deltas_impl(at, deltas, None)
    }

    /// 恢复退出时保留的日志。每分钟提交一个递增标记，同一日志重复恢复不重复计费。
    pub fn add_recovered_deltas(
        &mut self,
        at: SystemTime,
        deltas: &[(AppKey, u64, u64)],
        token: &str,
    ) -> Result<usize, StoreError> {
        self.add_deltas_impl(at, deltas, Some(token))
    }

    fn add_deltas_impl(
        &mut self,
        at: SystemTime,
        deltas: &[(AppKey, u64, u64)],
        recovery_token: Option<&str>,
    ) -> Result<usize, StoreError> {
        let secs = system_secs(at);
        let minute = secs - secs % 60;

        // 先按 (minute, app) 合并，避免同一批里对同一行反复 upsert
        let mut merged: HashMap<(&str, i64), (u64, u64)> = HashMap::new();
        for (app, rx, tx) in deltas {
            if *rx == 0 && *tx == 0 {
                continue;
            }
            let e = merged.entry((app.as_str(), minute)).or_insert((0, 0));
            e.0 = e.0.saturating_add(*rx);
            e.1 = e.1.saturating_add(*tx);
        }

        let txn = self.conn.transaction()?;
        if let Some(token) = recovery_token {
            // token 包含日志 ID 和分钟；与数据更新在同一事务内提交。
            let changed = txn.execute(
                "INSERT OR IGNORE INTO meta(k,v) VALUES (?1,'recovered')",
                [format!("recovery:{token}")],
            )?;
            if changed == 0 {
                return Ok(0);
            }
        }
        let mut rows = 0usize;
        {
            let mut stmt = txn.prepare_cached(
                r#"INSERT INTO samples (minute, app, rx, tx) VALUES (?1, ?2, ?3, ?4)
                   ON CONFLICT(minute, app) DO UPDATE SET
                       rx = MIN(MAX(CAST(rx AS INTEGER), 0), 9223372036854775807 - excluded.rx) + excluded.rx,
                       tx = MIN(MAX(CAST(tx AS INTEGER), 0), 9223372036854775807 - excluded.tx) + excluded.tx"#,
            )?;
            for ((app, minute), (rx, tx_bytes)) in merged {
                stmt.execute(params![minute, app, clamp(rx), clamp(tx_bytes)])?;
                rows += 1;
            }
        }
        txn.commit()?;
        Ok(rows)
    }

    /// 一段时间内按应用汇总，按总量降序。
    /// 单方向累计超过 SQLite INTEGER 上限时饱和为 i64::MAX。
    pub fn totals_between(&self, from: i64, to: i64) -> Result<Vec<AppTotal>, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            r#"SELECT app, tm_sum(CAST(rx AS INTEGER)) AS total_rx,
                          tm_sum(CAST(tx AS INTEGER)) AS total_tx FROM samples
               WHERE minute >= ?1 AND minute < ?2
               GROUP BY app
               ORDER BY total_rx + total_tx DESC"#,
        )?;
        let rows = stmt.query_map(params![from, to], |r| {
            Ok(AppTotal {
                app: r.get(0)?,
                rx: r.get::<_, i64>(1)?.max(0) as u64,
                tx: r.get::<_, i64>(2)?.max(0) as u64,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 按桶聚合总量，用来画曲线。`bucket_secs` 例如 60（1 小时视图）或 3600（24 小时视图）。
    pub fn timeline(
        &self,
        from: i64,
        to: i64,
        bucket_secs: i64,
    ) -> Result<Vec<Bucket>, StoreError> {
        let bucket = bucket_secs.max(1);
        let mut stmt = self.conn.prepare_cached(
            r#"SELECT (minute / ?3) * ?3 AS b,
                      tm_sum(CAST(rx AS INTEGER)), tm_sum(CAST(tx AS INTEGER)) FROM samples
               WHERE minute >= ?1 AND minute < ?2
               GROUP BY b
               ORDER BY b"#,
        )?;
        let rows = stmt.query_map(params![from, to, bucket], |r| {
            Ok(Bucket {
                ts: r.get(0)?,
                rx: r.get::<_, i64>(1)?.max(0) as u64,
                tx: r.get::<_, i64>(2)?.max(0) as u64,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 删掉 `before` 之前的数据，返回删除行数。
    pub fn prune_before(&mut self, before: i64) -> Result<usize, StoreError> {
        Ok(self
            .conn
            .execute("DELETE FROM samples WHERE minute < ?1", params![before])?)
    }

    /// 库里一共多少行，用于设置页显示占用。
    pub fn row_count(&self) -> Result<u64, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    /// 本地时间的紧凑时间戳，用作导出文件名：`20261117-1111`。
    pub fn local_now_stamp(&self) -> Result<String, StoreError> {
        Ok(self.conn.query_row(
            "SELECT strftime('%Y%m%d-%H%M', 'now', 'localtime')",
            [],
            |r| r.get(0),
        )?)
    }

    /// 导出 CSV。返回写入的数据行数（不含表头）；目标存在时返回 AlreadyExists。
    ///
    /// 时间戳交给 SQLite 的 `localtime` 转换：它用的是系统时区数据库，
    /// 夏令时切换、非整点时区都不用我们自己算。
    ///
    /// 文件开头写 UTF-8 BOM —— Windows 版 Excel 没有 BOM 会把中文列名读成乱码。
    pub fn export_csv(&self, from: i64, to: i64, path: &Path) -> Result<u64, StoreError> {
        // 快速跳过重名文件；提交时仍用 persist_noclobber 防止并发覆盖。
        if path.try_exists()? {
            return Err(
                std::io::Error::new(std::io::ErrorKind::AlreadyExists, "导出文件已存在").into(),
            );
        }
        let parent = files::parent_dir(path);
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        let mut file = std::io::BufWriter::new(temporary.as_file_mut());
        use std::io::Write;
        write!(file, "\u{feff}")?;
        // 同时给出展示名和原始标识：前者给人看，后者用来做精确筛选
        writeln!(file, "时间,应用,标识,下载字节,上传字节")?;

        let mut stmt = self.conn.prepare_cached(
            r#"SELECT datetime(minute, 'unixepoch', 'localtime') AS t, app,
                      CAST(rx AS INTEGER), CAST(tx AS INTEGER)
               FROM samples
               WHERE minute >= ?1 AND minute < ?2
               ORDER BY minute, rx + tx DESC"#,
        )?;
        let mut rows = stmt.query(params![from, to])?;
        let mut count = 0u64;
        while let Some(r) = rows.next()? {
            let t: String = r.get(0)?;
            let app: String = r.get(1)?;
            let rx: i64 = r.get(2)?;
            let tx: i64 = r.get(3)?;
            writeln!(
                file,
                "{},{},{},{},{}",
                t,
                csv_field(&tm_core::pretty_name(&app)),
                csv_field(&app),
                rx.max(0),
                tx.max(0)
            )?;
            count += 1;
        }
        file.flush()?;
        drop(file);
        temporary.as_file().sync_all()?;
        // 同一分钟重复导出也不能覆盖用户之前保存或编辑过的文件。
        temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
        Ok(count)
    }
}

/// CSV 字段转义，并阻止应用名被电子表格软件解释成公式。
/// 双引号只保护 CSV 结构，不会关闭 Excel 的公式解析，因此先加文本前缀。
fn csv_field(s: &str) -> String {
    let text;
    let s = if s.trim_start().starts_with(['=', '+', '-', '@']) || s.starts_with(['\t', '\n', '\r'])
    {
        text = format!("'{s}");
        text.as_str()
    } else {
        s
    };
    if s.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// 导出文件默认落到的目录。
pub fn default_export_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);

    if let Some(home) = home {
        let downloads = home.join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
        return home;
    }
    std::env::temp_dir()
}

/// 应用自己的数据目录，和数据库放在一起。
///
/// 用途之一是导出失败后的兜底：macOS 的 TCC 会在某些情况下拒绝写入
/// 「下载」目录（用户点了「不允许」），而应用自己的容器目录总是可写的。
pub fn data_dir() -> PathBuf {
    default_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn system_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// SQLite 的 INTEGER 是 i64，u64 极端值要夹一下，否则会变成负数。
fn clamp(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

/// 默认数据库位置。
pub fn default_path() -> PathBuf {
    // 原生运行验收使用独立目录，同时隔离历史、设置、日志和实例锁。
    if let Some(dir) = std::env::var_os("TM_DATA_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join("history.db");
    }
    if let Some(base) = system_data_base() {
        return base.join(tm_core::identity::APP_NAME).join("history.db");
    }
    PathBuf::from("history.db")
}

fn system_data_base() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    None
}

/// 新版本启动前导入旧数据；显式隔离目录/数据库的开发实例不接触用户数据。
pub fn prepare_data_dir() -> Result<(), StoreError> {
    if std::env::var_os("TM_DATA_DIR").is_some() || std::env::var_os("TM_DB_PATH").is_some() {
        return Ok(());
    }
    if let Some(base) = system_data_base() {
        migration::migrate(&base)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn csv_export_preserves_existing_files_and_cleans_failed_exports() {
        let store = Store::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.csv");
        std::fs::write(&path, b"user edited export").unwrap();
        assert!(store.export_csv(0, i64::MAX, &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"user edited export");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        store.conn.execute("DROP TABLE samples", []).unwrap();
        let failed = dir.path().join("failed.csv");
        assert!(store.export_csv(0, i64::MAX, &failed).is_err());
        assert!(!failed.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn read_only_export_works_while_another_writer_holds_a_transaction() {
        let dir = std::env::temp_dir().join(format!(
            "tm-read-only-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = dir.join("history.db");
        let csv = dir.join("history.csv");
        {
            let mut writer = Store::open(&db).unwrap();
            writer
                .add_deltas(at(60), &[(app("browser"), 123, 7)])
                .unwrap();
            writer
                .conn
                .execute_batch("BEGIN IMMEDIATE; UPDATE samples SET rx = 999;")
                .unwrap();
            let reader = Store::open_read_only(&db).unwrap();
            assert_eq!(reader.totals_between(0, 120).unwrap()[0].rx, 123);
            assert_eq!(reader.export_csv(0, 120, &csv).unwrap(), 1);
            assert!(std::fs::read_to_string(&csv).unwrap().contains(",123,7"));
            assert!(reader.conn.execute("DELETE FROM samples", []).is_err());
            writer.conn.execute_batch("ROLLBACK").unwrap();
        }
        std::fs::remove_file(csv).unwrap();
        std::fs::remove_file(db).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn read_only_open_never_creates_a_missing_database() {
        let path = std::env::temp_dir().join(format!(
            "tm-missing-{}-{}.db",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(Store::open_read_only(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn recovery_replay_is_idempotent() {
        let mut store = Store::open_memory().unwrap();
        let delta = [(AppKey::from("app"), 123, 9)];
        assert_eq!(
            store
                .add_recovered_deltas(at(60), &delta, "crash-1:60")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .add_recovered_deltas(at(60), &delta, "crash-1:60")
                .unwrap(),
            0
        );
        assert_eq!(store.totals_between(0, 120).unwrap()[0].rx, 123);
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn app(s: &str) -> AppKey {
        AppKey::new(s.to_owned())
    }

    #[test]
    fn writes_and_reads_totals() {
        let mut s = Store::open_memory().unwrap();
        let now = 1_700_000_000;
        s.add_deltas(
            at(now),
            &[(app("chrome"), 1000, 100), (app("slack"), 50, 5)],
        )
        .unwrap();

        let totals = s.totals_between(now as i64 - 60, now as i64 + 60).unwrap();
        assert_eq!(totals.len(), 2);
        assert_eq!(totals[0].app, "chrome", "应按总量降序");
        assert_eq!(totals[0].rx, 1000);
        assert_eq!(totals[1].app, "slack");
    }

    #[test]
    fn same_minute_accumulates() {
        let mut s = Store::open_memory().unwrap();
        let now = 1_700_000_030; // 同一分钟内
        s.add_deltas(at(now), &[(app("chrome"), 100, 0)]).unwrap();
        s.add_deltas(at(now + 5), &[(app("chrome"), 200, 0)])
            .unwrap();

        let totals = s.totals_between(now as i64 - 60, now as i64 + 120).unwrap();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].rx, 300, "同一分钟应累加而不是覆盖");
    }

    #[test]
    fn zero_deltas_are_skipped() {
        let mut s = Store::open_memory().unwrap();
        let n = s
            .add_deltas(at(1_700_000_000), &[(app("chrome"), 0, 0)])
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(s.row_count().unwrap(), 0);
    }

    #[test]
    fn timeline_buckets_by_hour() {
        let mut s = Store::open_memory().unwrap();
        let base = 1_700_000_000;
        // 三个不同的分钟，都落在同一小时内
        for i in 0..3 {
            s.add_deltas(at(base + i * 60), &[(app("chrome"), 100, 0)])
                .unwrap();
        }
        let buckets = s
            .timeline(base as i64 - 60, base as i64 + 3600, 3600)
            .unwrap();
        assert_eq!(buckets.len(), 1, "都在同一小时内");
        assert_eq!(buckets[0].rx, 300);
    }

    #[test]
    fn prune_removes_old_rows() {
        let mut s = Store::open_memory().unwrap();
        s.add_deltas(at(1_000_000_000), &[(app("old"), 1, 1)])
            .unwrap();
        s.add_deltas(at(1_700_000_000), &[(app("new"), 1, 1)])
            .unwrap();

        let removed = s.prune_before(1_500_000_000).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.row_count().unwrap(), 1);
    }

    #[test]
    fn huge_values_are_clamped_not_negative() {
        let mut s = Store::open_memory().unwrap();
        s.add_deltas(at(1_700_000_000), &[(app("x"), u64::MAX, 0)])
            .unwrap();
        let t = s.totals_between(1_699_999_000, 1_700_001_000).unwrap();
        assert_eq!(t[0].rx, i64::MAX as u64);
    }

    #[test]
    fn repeated_large_deltas_and_aggregates_saturate_without_breaking_exports() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("history.db");
        let mut s = Store::open(&database).unwrap();
        let limit = i64::MAX as u64;
        s.add_deltas(at(60), &[(app("x"), limit - 1, limit - 1)])
            .unwrap();
        s.add_deltas(at(61), &[(app("x"), 10, 10)]).unwrap();
        s.add_deltas(at(120), &[(app("x"), 10, 10)]).unwrap();
        s.add_deltas(at(60), &[(app("y"), 10, 10)]).unwrap();
        let types: (String, String) = s
            .conn
            .query_row(
                "SELECT typeof(rx), typeof(tx) FROM samples WHERE minute = 60 AND app = 'x'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(types, ("integer".into(), "integer".into()));
        drop(s);

        // 只读连接也必须支持相同的汇总，不能仅在写入连接注册聚合函数。
        let s = Store::open_read_only(&database).unwrap();
        assert_eq!(
            s.totals_between(0, 180).unwrap()[0],
            AppTotal {
                app: "x".into(),
                rx: limit,
                tx: limit,
            }
        );
        assert_eq!(
            s.timeline(0, 180, 3600).unwrap(),
            [Bucket {
                ts: 0,
                rx: limit,
                tx: limit,
            }]
        );
        let export = dir.path().join("history.csv");
        assert_eq!(s.export_csv(0, 180, &export).unwrap(), 3);
        assert!(std::fs::read_to_string(export)
            .unwrap()
            .contains(&format!(",{limit},{limit}")));
    }

    #[test]
    fn aggregation_preserves_integer_precision_above_f64_exact_range() {
        let mut s = Store::open_memory().unwrap();
        let large = (1u64 << 53) + 1;
        s.add_deltas(at(60), &[(app("x"), large, 0)]).unwrap();
        s.add_deltas(at(120), &[(app("x"), 2, 0)]).unwrap();
        assert_eq!(s.totals_between(0, 180).unwrap()[0].rx, large + 2);
        assert_eq!(s.timeline(0, 180, 3600).unwrap()[0].rx, large + 2);
    }

    #[test]
    fn legacy_overflowed_rows_remain_readable_and_can_be_updated() {
        let mut s = Store::open_memory().unwrap();
        // 旧版 upsert 在整数溢出后会将 SQLite 列变成 REAL。
        s.conn
            .execute(
                "INSERT INTO samples VALUES (60, 'x', 9223372036854775807 + 1, 0)",
                [],
            )
            .unwrap();
        assert_eq!(s.totals_between(0, 120).unwrap()[0].rx, i64::MAX as u64);
        assert_eq!(s.timeline(0, 120, 60).unwrap()[0].rx, i64::MAX as u64);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            s.export_csv(0, 120, &dir.path().join("history.csv"))
                .unwrap(),
            1
        );
        s.add_deltas(at(61), &[(app("x"), 1, 0)]).unwrap();
        assert_eq!(s.totals_between(0, 120).unwrap()[0].rx, i64::MAX as u64);
    }

    #[test]
    fn empty_range_returns_nothing() {
        let s = Store::open_memory().unwrap();
        assert!(s.totals_between(0, 100).unwrap().is_empty());
        assert!(s.timeline(0, 100, 60).unwrap().is_empty());
    }

    #[test]
    fn export_writes_bom_header_and_rows() {
        let mut s = Store::open_memory().unwrap();
        s.add_deltas(at(1_700_000_000), &[(app("chrome"), 1000, 100)])
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        let n = s.export_csv(0, i64::MAX, &path).unwrap();
        assert_eq!(n, 1);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            &bytes[..3],
            &[0xEF, 0xBB, 0xBF],
            "必须以 UTF-8 BOM 开头，否则 Excel 读中文是乱码"
        );
        let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), "时间,应用,标识,下载字节,上传字节");
        let row = lines.next().unwrap();
        assert!(
            row.ends_with(",chrome,chrome,1000,100"),
            "展示名和标识都要有，实际：{row}"
        );
        // datetime(...) 出来的形如 2023-11-14 22:13:00，本地时区不同但格式固定
        assert!(row.starts_with("20"), "时间列应是可读时间：{row}");
    }

    #[test]
    fn csv_field_escapes_separators() {
        assert_eq!(csv_field("chrome"), "chrome");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn csv_export_keeps_formula_like_application_names_as_text() {
        for name in ["=1+1", "+1+1", "-1+1", "@SUM(1)", "  =1+1", "\t=1+1"] {
            assert_eq!(csv_field(name), format!("'{name}"));
        }
        assert_eq!(csv_field("\r=1+1"), "\"'\r=1+1\"");
        assert_eq!(csv_field("=SUM(1,2)"), "\"'=SUM(1,2)\"");

        let mut store = Store::open_memory().unwrap();
        store
            .add_deltas(at(60), &[(app("/Applications/=1+1.app"), 10, 0)])
            .unwrap();
        store.add_deltas(at(60), &[(app("=1+1"), 20, 0)]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.csv");
        store.export_csv(0, 120, &path).unwrap();
        let csv = std::fs::read_to_string(path).unwrap();
        assert!(csv.contains(",'=1+1,/Applications/=1+1.app,10,0"));
        assert!(csv.contains(",'=1+1,'=1+1,20,0"));
    }
}
