use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
    types::ValueRef,
};
use serde_json::Value;

use crate::{
    contracts::{AccountCatalog, AccountKey, DiscoveredAccount},
    error::AppError,
};

const FOREGROUND_COPY_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_COPY_TIMEOUT: Duration = Duration::from_millis(500);

pub fn read_account_catalog(database_path: &Path) -> Result<AccountCatalog, AppError> {
    read_account_catalog_with_timeout(database_path, FOREGROUND_COPY_TIMEOUT)
}

pub fn read_account_catalog_for_probe(database_path: &Path) -> Result<AccountCatalog, AppError> {
    read_account_catalog_with_timeout(database_path, PROBE_COPY_TIMEOUT)
}

fn read_account_catalog_with_timeout(
    database_path: &Path,
    copy_timeout: Duration,
) -> Result<AccountCatalog, AppError> {
    if !database_path.is_file() {
        return Err(AppError::AccountDatabaseMissing);
    }

    let source = Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| AppError::AccountDatabase(error.to_string()))?;
    source
        .busy_timeout(Duration::from_millis(100))
        .map_err(|error| AppError::AccountDatabase(error.to_string()))?;

    let mut consistent = Connection::open_in_memory()
        .map_err(|error| AppError::AccountDatabase(error.to_string()))?;
    copy_database_with_timeout(&source, &mut consistent, copy_timeout)?;

    read_catalog_from_connection(&consistent)
}

fn copy_database_with_timeout(
    source: &Connection,
    destination: &mut Connection,
    timeout: Duration,
) -> Result<(), AppError> {
    let backup = Backup::new(source, destination)
        .map_err(|error| AppError::AccountDatabase(error.to_string()))?;
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(AppError::AccountDatabase(
                "战网账号数据库正忙，请稍后重试".into(),
            ));
        }
        match backup
            .step(64)
            .map_err(|error| AppError::AccountDatabase(error.to_string()))?
        {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            StepResult::Busy | StepResult::Locked => {
                thread::sleep(Duration::from_millis(20));
            }
            _ => {
                return Err(AppError::AccountDatabase(
                    "战网账号数据库返回了不支持的备份状态".into(),
                ));
            }
        }
    }
}

fn read_catalog_from_connection(connection: &Connection) -> Result<AccountCatalog, AppError> {
    verify_schema(connection)?;
    let mut statement = connection
        .prepare(
            "SELECT name, environment, battle_tag, account_id_hi, account_id_lo FROM login_cache",
        )
        .map_err(|error| AppError::AccountDatabaseIncompatible(error.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            let account_id_hi_value = row.get_ref(3)?;
            let account_id_hi = value_as_id(account_id_hi_value).ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    3,
                    "account_id_hi".into(),
                    account_id_hi_value.data_type(),
                )
            })?;
            let account_id_lo_value = row.get_ref(4)?;
            let account_id_lo = value_as_id(account_id_lo_value).ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    4,
                    "account_id_lo".into(),
                    account_id_lo_value.data_type(),
                )
            })?;
            let environment = row
                .get::<_, Option<String>>(1)?
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            Ok(DiscoveredAccount {
                key: AccountKey {
                    environment,
                    account_id: format!("{}:{}", account_id_hi.trim(), account_id_lo.trim()),
                },
                internal_name: row
                    .get::<_, Option<String>>(0)?
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                battle_tag: row
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            })
        })
        .map_err(|error| AppError::AccountDatabase(error.to_string()))?;

    let mut accounts = BTreeMap::new();
    for row in rows {
        let account = row.map_err(|error| AppError::AccountDatabase(error.to_string()))?;
        if account_id_low(&account.key.account_id) != Some("0")
            && !account.key.environment.trim().is_empty()
        {
            accounts.insert(account.key.clone(), account);
        }
    }

    let active_id = read_active_account_id(connection)?;
    let current_account_key = active_id
        .map(|active_id| {
            let matches = accounts
                .keys()
                .filter(|key| account_id_low(&key.account_id) == Some(active_id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => Ok(None),
                [key] => Ok(Some(key.clone())),
                _ => Err(AppError::AccountDatabaseIncompatible(
                    "当前账号 ID 同时属于多个战网环境，无法安全判断当前账号".into(),
                )),
            }
        })
        .transpose()?
        .flatten();

    Ok(AccountCatalog {
        accounts: accounts.into_values().collect(),
        current_account_key,
    })
}

fn verify_schema(connection: &Connection) -> Result<(), AppError> {
    for (table, required_columns) in [
        (
            "login_cache",
            &[
                "name",
                "environment",
                "battle_tag",
                "account_id_hi",
                "account_id_lo",
            ][..],
        ),
        ("key_value_store", &["key", "value"][..]),
    ] {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|error| AppError::AccountDatabaseIncompatible(error.to_string()))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| AppError::AccountDatabaseIncompatible(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::AccountDatabaseIncompatible(error.to_string()))?;
        if required_columns
            .iter()
            .any(|required| !columns.iter().any(|column| column == required))
        {
            return Err(AppError::AccountDatabaseIncompatible(format!(
                "表 {table} 缺少必要字段"
            )));
        }
    }
    Ok(())
}

fn read_active_account_id(connection: &Connection) -> Result<Option<String>, AppError> {
    let mut statement = connection
        .prepare("SELECT value FROM key_value_store WHERE key = 'features_cached_data_points'")
        .map_err(|error| AppError::AccountDatabase(error.to_string()))?;
    let values = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| AppError::AccountDatabase(error.to_string()))?;
    let mut active_ids = BTreeSet::new();
    let mut saw_row_without_account = false;
    for value in values {
        let value = value.map_err(|error| AppError::AccountDatabase(error.to_string()))?;
        let document: Value = serde_json::from_str(&value).map_err(|error| {
            AppError::AccountDatabaseIncompatible(format!("当前账号字段不是有效 JSON：{error}"))
        })?;
        match document.get("account_id").and_then(json_value_as_id) {
            Some(account_id) => {
                active_ids.insert(account_id);
            }
            None => saw_row_without_account = true,
        }
    }
    if active_ids.len() > 1 || (saw_row_without_account && !active_ids.is_empty()) {
        return Err(AppError::AccountDatabaseIncompatible(
            "当前账号记录包含互相矛盾的值，无法安全判断当前账号".into(),
        ));
    }
    let Some(active_id) = active_ids.into_iter().next() else {
        return Ok(None);
    };
    Ok(Some(active_id))
}

fn value_as_id(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) if value.fract() == 0.0 => Some(format!("{value:.0}")),
        ValueRef::Text(value) => std::str::from_utf8(value).ok().map(str::to_owned),
        _ => None,
    }
}

fn json_value_as_id(value: &Value) -> Option<String> {
    value
        .as_i64()
        .map(|value| value.to_string())
        .or_else(|| value.as_u64().map(|value| value.to_string()))
        .or_else(|| value.as_str().map(str::to_owned))
}

fn account_id_low(account_id: &str) -> Option<&str> {
    let (high, low) = account_id.split_once(':')?;
    (!high.is_empty() && !low.is_empty() && !low.contains(':')).then_some(low)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        copy_database_with_timeout, read_account_catalog, read_account_catalog_for_probe,
        read_catalog_from_connection,
    };

    #[test]
    fn reads_accounts_and_active_account_from_fixture() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE login_cache(name TEXT, environment TEXT, battle_tag TEXT, account_id_hi INTEGER, account_id_lo INTEGER);\
             CREATE TABLE key_value_store(key TEXT, value TEXT);\
             INSERT INTO login_cache VALUES('one', 'cn', 'Test#1234', 7, 42);\
             INSERT INTO key_value_store VALUES('features_cached_data_points', '{\"account_id\":42}');",
        ).unwrap();

        let catalog = read_catalog_from_connection(&connection).unwrap();
        assert_eq!(catalog.accounts.len(), 1);
        assert_eq!(catalog.accounts[0].battle_tag, "Test#1234");
        assert_eq!(catalog.current_account_key.unwrap().account_id, "7:42");
    }

    #[test]
    fn rejects_ambiguous_active_account_across_environments() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE login_cache(name TEXT, environment TEXT, battle_tag TEXT, account_id_hi INTEGER, account_id_lo INTEGER);\
             CREATE TABLE key_value_store(key TEXT, value TEXT);\
             INSERT INTO login_cache VALUES('one', 'CN', 'One#1', 7, 42);\
             INSERT INTO login_cache VALUES('two', 'global', 'Two#2', 8, 42);\
             INSERT INTO key_value_store VALUES('features_cached_data_points', '{\"account_id\":42}');",
        ).unwrap();

        assert!(read_catalog_from_connection(&connection).is_err());
    }

    #[test]
    fn rejects_conflicting_active_account_rows() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE login_cache(name TEXT, environment TEXT, battle_tag TEXT, account_id_hi INTEGER, account_id_lo INTEGER);\
             CREATE TABLE key_value_store(key TEXT, value TEXT);\
             INSERT INTO login_cache VALUES('one', 'cn', 'One#1', 7, 42);\
             INSERT INTO login_cache VALUES('two', 'cn', 'Two#2', 8, 43);\
             INSERT INTO key_value_store VALUES('features_cached_data_points', '{\"account_id\":42}');\
             INSERT INTO key_value_store VALUES('features_cached_data_points', '{\"account_id\":43}');",
        ).unwrap();

        assert!(read_catalog_from_connection(&connection).is_err());
    }

    #[test]
    fn accepts_duplicate_identical_active_account_rows() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE login_cache(name TEXT, environment TEXT, battle_tag TEXT, account_id_hi INTEGER, account_id_lo INTEGER);\
             CREATE TABLE key_value_store(key TEXT, value TEXT);\
             INSERT INTO login_cache VALUES('one', 'cn', 'One#1', 7, 42);\
             INSERT INTO key_value_store VALUES('features_cached_data_points', '{\"account_id\":42}');\
             INSERT INTO key_value_store VALUES('features_cached_data_points', '{\"account_id\":42}');",
        ).unwrap();

        let catalog = read_catalog_from_connection(&connection).unwrap();
        assert_eq!(catalog.current_account_key.unwrap().account_id, "7:42");
    }

    #[test]
    fn reads_committed_wal_data_without_copying_sidecar_files() {
        let temporary = tempfile::tempdir().unwrap();
        let database_path = temporary.path().join("CachedData.db");
        let writer = rusqlite::Connection::open(&database_path).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer.execute_batch(
            "CREATE TABLE login_cache(name TEXT, environment TEXT, battle_tag TEXT, account_id_hi INTEGER, account_id_lo INTEGER);\
             CREATE TABLE key_value_store(key TEXT, value TEXT);\
             INSERT INTO login_cache VALUES('one', 'GLOBAL', 'Wal#1234', 9, 77);\
             INSERT INTO key_value_store VALUES('features_cached_data_points', '{\"account_id\":77}');",
        ).unwrap();

        let catalog = read_account_catalog(&database_path).unwrap();
        assert_eq!(catalog.accounts.len(), 1);
        assert_eq!(catalog.accounts[0].key.environment, "global");
        assert_eq!(catalog.current_account_key.unwrap().account_id, "9:77");

        let probe_catalog = read_account_catalog_for_probe(&database_path).unwrap();
        assert_eq!(probe_catalog.accounts.len(), 1);
        assert_eq!(
            probe_catalog.current_account_key.unwrap().account_id,
            "9:77"
        );
        drop(writer);
    }

    #[test]
    fn backup_has_a_total_deadline() {
        let source = rusqlite::Connection::open_in_memory().unwrap();
        let mut destination = rusqlite::Connection::open_in_memory().unwrap();
        let error = copy_database_with_timeout(&source, &mut destination, Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert!(error.contains("数据库正忙"));
    }
}
