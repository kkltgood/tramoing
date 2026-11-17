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
//! 存增量而不是累计值，查询时直接 `SUM` 就行，不用处理「进程重启导致累计归零」。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use tm_core::AppKey;

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

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.migrate()?;
        // WAL 让「后台写入」和「界面查询」不会互相卡住
        let _ = store.conn.pragma_update(None, "journal_mode", "WAL");
        let _ = store.conn.pragma_update(None, "synchronous", "NORMAL");
        Ok(store)
    }

    /// 只用于测试。
    pub fn open_memory() -> Result<Self, StoreError> {
        let store = Self {
            conn: Connection::open_in_memory()?,
        };
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
        let mut rows = 0usize;
        {
            let mut stmt = txn.prepare_cached(
                r#"INSERT INTO samples (minute, app, rx, tx) VALUES (?1, ?2, ?3, ?4)
                   ON CONFLICT(minute, app) DO UPDATE SET
                       rx = rx + excluded.rx,
                       tx = tx + excluded.tx"#,
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
    pub fn totals_between(&self, from: i64, to: i64) -> Result<Vec<AppTotal>, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            r#"SELECT app, SUM(rx), SUM(tx) FROM samples
               WHERE minute >= ?1 AND minute < ?2
               GROUP BY app
               ORDER BY SUM(rx) + SUM(tx) DESC"#,
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
    pub fn timeline(&self, from: i64, to: i64, bucket_secs: i64) -> Result<Vec<Bucket>, StoreError> {
        let bucket = bucket_secs.max(1);
        let mut stmt = self.conn.prepare_cached(
            r#"SELECT (minute / ?3) * ?3 AS b, SUM(rx), SUM(tx) FROM samples
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

    /// 导出 CSV。返回写入的数据行数（不含表头）。
    ///
    /// 时间戳交给 SQLite 的 `localtime` 转换：它用的是系统时区数据库，
    /// 夏令时切换、非整点时区都不用我们自己算。
    ///
    /// 文件开头写 UTF-8 BOM —— Windows 版 Excel 没有 BOM 会把中文列名读成乱码。
    pub fn export_csv(&self, from: i64, to: i64, path: &Path) -> Result<u64, StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        use std::io::Write;
        write!(file, "\u{feff}")?;
        // 同时给出展示名和原始标识：前者给人看，后者用来做精确筛选
        writeln!(file, "时间,应用,标识,下载字节,上传字节")?;

        let mut stmt = self.conn.prepare_cached(
            r#"SELECT datetime(minute, 'unixepoch', 'localtime') AS t, app, rx, tx
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
        Ok(count)
    }
}

/// CSV 字段转义：含逗号、引号、换行时用双引号包起来，内部引号翻倍。
fn csv_field(s: &str) -> String {
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
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home)
                .join("Library/Application Support/TrafficMonitor")
                .join("history.db");
        }
    }
    #[cfg(windows)]
    {
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            return Path::new(&base).join("TrafficMonitor").join("history.db");
        }
    }
    PathBuf::from("history.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
        s.add_deltas(at(now), &[(app("chrome"), 1000, 100), (app("slack"), 50, 5)])
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

        let dir = std::env::temp_dir().join("tm-test-export");
        let path = dir.join("out.csv");
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn csv_field_escapes_separators() {
        assert_eq!(csv_field("chrome"), "chrome");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }
}
