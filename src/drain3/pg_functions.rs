use pgrx::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;

use super::types::{DrainConfig, DrainState, LogCluster};

/// Reserved JSONB key for the positional values array in drain3_extract_params output.
/// Using "_values" (underscore prefix = reserved) avoids conflict with user-defined
/// wildcard names like <user> or <ip>, and works cleanly with jsonpath.
pub const PARAMS_VALUES_KEY: &str = "_values";

// -- Session-level model storage --
// Each PG backend is single-threaded, so RefCell is safe.
thread_local! {
    static MODELS: RefCell<HashMap<String, ModelSession>> = RefCell::new(HashMap::new());
}

struct ModelSession {
    state: DrainState,
    table: Option<String>, // bound table name for dump
}

fn with_model<F, R>(name: &str, f: F) -> R
where
    F: FnOnce(&DrainState) -> R,
{
    MODELS.with(|m| {
        let models = m.borrow();
        match models.get(name) {
            Some(session) => f(&session.state),
            None => pgrx::error!("model '{name}' not loaded, call drain3_load first"),
        }
    })
}

fn with_model_mut<F, R>(name: &str, f: F) -> R
where
    F: FnOnce(&mut DrainState) -> R,
{
    MODELS.with(|m| {
        let mut models = m.borrow_mut();
        match models.get_mut(name) {
            Some(session) => f(&mut session.state),
            None => pgrx::error!("model '{name}' not loaded, call drain3_load first"),
        }
    })
}

/// PostgreSQL custom type wrapping DrainState (kept for backward compat).
#[derive(Debug, Clone, Serialize, Deserialize, PostgresType)]
pub struct PgDrainState(pub DrainState);

// ============================================================
// Session-based API: load / feed / match / clusters / dump
// ============================================================

/// Load or create a model into session memory.
///
/// ```sql
/// SELECT drain3_load('syslog');
/// SELECT drain3_load('syslog', config := '{"depth": 3}');
/// SELECT drain3_load('syslog', config := '{"depth": 3}', table_name := 'my_models');
/// ```
#[pg_extern]
fn drain3_load(
    name: &str,
    config: default!(Option<pgrx::JsonB>, "NULL"),
    table_name: default!(Option<&str>, "NULL"),
) -> i64 {
    let cfg: DrainConfig = config
        .and_then(|j| serde_json::from_value(j.0).ok())
        .unwrap_or_default();

    // Try to load from table if specified
    let (state, bound_table) = if let Some(tbl) = table_name {
        let escaped_tbl = tbl.replace('"', "\"\"");
        let escaped_name = name.replace('\'', "''");

        // Check if table exists and has data
        let clusters = load_clusters_from_table(&escaped_tbl, &escaped_name);

        if clusters.is_empty() {
            (DrainState::new(cfg), Some(tbl.to_string()))
        } else {
            (DrainState::from_clusters(cfg, clusters), Some(tbl.to_string()))
        }
    } else {
        (DrainState::new(cfg), None)
    };

    let cluster_count = state.clusters.len() as i64;

    MODELS.with(|m| {
        m.borrow_mut().insert(
            name.to_string(),
            ModelSession {
                state,
                table: bound_table,
            },
        );
    });

    cluster_count
}

/// Feed a log message into a loaded model. Pure in-memory, zero SPI overhead.
///
/// ```sql
/// SELECT drain3_feed('syslog', line) FROM huge_logs;
/// ```
#[pg_extern]
fn drain3_feed(name: &str, log_message: &str) -> i64 {
    with_model_mut(name, |state| state.add_log_message(log_message) as i64)
}

/// Match a log message against a loaded model.
///
/// ```sql
/// SELECT drain3_match('syslog', 'User admin logged in');
/// ```
#[pg_extern]
fn drain3_match_model(name: &str, log_message: &str) -> Option<String> {
    with_model(name, |state| {
        state
            .match_log_message(log_message)
            .map(|c| c.get_template())
    })
}

/// List clusters of a loaded model.
///
/// ```sql
/// SELECT * FROM drain3_clusters('syslog');
/// ```
#[pg_extern]
fn drain3_model_clusters(
    name: &str,
) -> TableIterator<
    'static,
    (
        name!(cluster_id, i64),
        name!(template, String),
        name!(size, i64),
        name!(examples, Vec<String>),
    ),
> {
    let rows: Vec<_> = with_model(name, |state| {
        state
            .get_clusters()
            .iter()
            .map(|c| {
                (
                    c.cluster_id as i64,
                    c.get_template(),
                    c.size as i64,
                    c.examples.clone(),
                )
            })
            .collect()
    });
    TableIterator::new(rows)
}

/// Dump (save) a loaded model to a table via UPSERT.
/// Uses the table bound at load time, or specify one.
///
/// ```sql
/// SELECT drain3_dump('syslog');
/// SELECT drain3_dump('syslog', table_name := 'other_table');
/// ```
#[pg_extern]
fn drain3_dump(name: &str, table_name: default!(Option<&str>, "NULL")) -> i64 {
    let (clusters, bound_table) = MODELS.with(|m| {
        let models = m.borrow();
        match models.get(name) {
            Some(session) => (session.state.clusters.clone(), session.table.clone()),
            None => pgrx::error!("model '{name}' not loaded"),
        }
    });

    let tbl = table_name
        .map(|s| s.to_string())
        .or(bound_table)
        .unwrap_or_else(|| pgrx::error!("no table specified, pass table_name or bind at load"));

    let escaped_tbl = tbl.replace('"', "\"\"");
    let escaped_name = name.replace('\'', "''");

    // Delete existing rows for this model, then insert all
    Spi::run(&format!(
        "DELETE FROM \"{escaped_tbl}\" WHERE model = '{escaped_name}'"
    ))
    .unwrap_or_else(|e| pgrx::error!("drain3_dump delete failed: {e}"));

    for cluster in &clusters {
        let tmpl_escaped = cluster.get_template().replace('\'', "''");
        let examples_sql = cluster
            .examples
            .iter()
            .map(|e| format!("'{}'", e.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");

        Spi::run(&format!(
            "INSERT INTO \"{escaped_tbl}\" (model, id, template, size, examples) VALUES ('{escaped_name}', {}, '{tmpl_escaped}', {}, ARRAY[{examples_sql}]::text[])",
            cluster.cluster_id,
            cluster.size,
        ))
        .unwrap_or_else(|e| pgrx::error!("drain3_dump insert failed: {e}"));
    }

    clusters.len() as i64
}

/// Unload a model from session memory.
#[pg_extern]
fn drain3_unload(name: &str) -> bool {
    MODELS.with(|m| m.borrow_mut().remove(name).is_some())
}

/// List all loaded models in current session.
#[pg_extern]
fn drain3_models() -> TableIterator<
    'static,
    (
        name!(name, String),
        name!(clusters, i64),
        name!(table_name, Option<String>),
    ),
> {
    let rows: Vec<_> = MODELS.with(|m| {
        m.borrow()
            .iter()
            .map(|(name, session)| {
                (
                    name.clone(),
                    session.state.clusters.len() as i64,
                    session.table.clone(),
                )
            })
            .collect()
    });
    TableIterator::new(rows)
}

// ============================================================
// Value-based API (kept for composability)
// ============================================================

/// Match a log message against a state value.
#[pg_extern(immutable)]
fn drain3_match(state: PgDrainState, log_message: &str) -> Option<String> {
    state
        .0
        .match_log_message(log_message)
        .map(|c| c.get_template())
}

/// List clusters from a state value.
#[pg_extern(immutable)]
fn drain3_clusters(
    state: PgDrainState,
) -> TableIterator<
    'static,
    (
        name!(cluster_id, i64),
        name!(template, String),
        name!(size, i64),
        name!(examples, Vec<String>),
    ),
> {
    let rows: Vec<_> = state
        .0
        .get_clusters()
        .iter()
        .map(|c| {
            (
                c.cluster_id as i64,
                c.get_template(),
                c.size as i64,
                c.examples.clone(),
            )
        })
        .collect();
    TableIterator::new(rows)
}

/// State transition function for the `drain3_mine` aggregate.
#[pg_extern]
fn drain3_mine_sfunc(state: Option<PgDrainState>, log_message: &str) -> Option<PgDrainState> {
    let mut s = state.unwrap_or_else(|| PgDrainState(DrainState::new(DrainConfig::default())));
    s.0.add_log_message(log_message);
    Some(s)
}

extension_sql!(
    r#"
CREATE AGGREGATE drain3_mine(text) (
    SFUNC = drain3_mine_sfunc,
    STYPE = pgdrainstate
);
"#,
    name = "drain3_mine_aggregate",
    requires = [drain3_mine_sfunc]
);

/// Extract parameters as JSONB.
///
/// Always returns `values` array for positional access.
/// Named wildcards like `<user>` are also merged at the top level.
///
/// ```sql
/// SELECT drain3_extract_params('User <user> logged in from <ip>', msg);
/// -- {"values": ["admin", "1.2.3.4"], "user": "admin", "ip": "1.2.3.4"}
///
/// SELECT drain3_extract_params('User <*> logged in from <*>', msg);
/// -- {"values": ["admin", "1.2.3.4"]}
///
/// -- Positional access
/// SELECT params->'values'->0           -- "admin"
/// -- Named access
/// SELECT params->>'user'               -- "admin"
/// ```
#[pg_extern(immutable)]
fn drain3_extract_params(template: &str, log_message: &str) -> pgrx::JsonB {
    let params = DrainState::extract_params(template, log_message);

    let values: Vec<serde_json::Value> = params
        .iter()
        .map(|(_, v)| serde_json::Value::String(v.clone()))
        .collect();

    let mut map = serde_json::Map::new();
    map.insert(PARAMS_VALUES_KEY.to_string(), serde_json::Value::Array(values));

    // Merge named params at top level (skip auto-indexed _1, _2, ...)
    for (k, v) in &params {
        if !k.starts_with('_') {
            map.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
    }

    pgrx::JsonB(serde_json::Value::Object(map))
}

// ============================================================
// Internal helpers
// ============================================================

fn load_clusters_from_table(escaped_tbl: &str, escaped_name: &str) -> Vec<LogCluster> {
    let mut clusters = Vec::new();

    let query = format!(
        "SELECT id, template, size, examples FROM \"{escaped_tbl}\" WHERE model = '{escaped_name}' ORDER BY id"
    );

    // Check if table exists first
    let table_exists = Spi::get_one::<bool>(&format!(
        "SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname = '{escaped_tbl}' AND relkind = 'r')"
    ))
    .unwrap_or(Some(false))
    .unwrap_or(false);

    if !table_exists {
        return clusters;
    }

    Spi::connect(|client| {
        let result = client.select(&query, None, &[]);
        if let Ok(tup_table) = result {
            for row in tup_table {
                let id: i64 = row.get_by_name("id").unwrap().unwrap_or(0);
                let template: String = row.get_by_name("template").unwrap().unwrap_or_default();
                let size: i64 = row.get_by_name("size").unwrap().unwrap_or(0);
                let examples: Vec<String> = row
                    .get_by_name("examples")
                    .unwrap()
                    .unwrap_or_default();

                clusters.push(LogCluster {
                    cluster_id: id as usize,
                    template_tokens: template.split_whitespace().map(String::from).collect(),
                    size: size as usize,
                    examples,
                });
            }
        }
    });

    clusters
}

// ============================================================
// Tests
// ============================================================

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn test_drain3_load_feed_match() {
        Spi::run("SELECT drain3_load('test', config := '{\"depth\": 3}')").unwrap();
        Spi::run("SELECT drain3_feed('test', 'User alice logged in')").unwrap();
        Spi::run("SELECT drain3_feed('test', 'User bob logged in')").unwrap();
        Spi::run("SELECT drain3_feed('test', 'User charlie logged in')").unwrap();

        let tmpl = Spi::get_one::<String>(
            "SELECT drain3_match_model('test', 'User dave logged in')",
        )
        .unwrap();
        assert!(tmpl.is_some());
        let t = tmpl.unwrap();
        assert!(t.contains("User"));
        assert!(t.contains("<*>"));
    }

    #[pg_test]
    fn test_drain3_clusters_with_examples() {
        Spi::run("SELECT drain3_load('test2', config := '{\"depth\": 3}')").unwrap();
        Spi::run("SELECT drain3_feed('test2', 'Request took 100 ms')").unwrap();
        Spi::run("SELECT drain3_feed('test2', 'Request took 200 ms')").unwrap();

        let examples = Spi::get_one::<Vec<String>>(
            "SELECT examples FROM drain3_model_clusters('test2') LIMIT 1",
        )
        .unwrap();
        assert!(examples.is_some());
        let ex = examples.unwrap();
        assert!(!ex.is_empty());
        assert!(ex[0].contains("Request took"));
    }

    #[pg_test]
    fn test_drain3_dump_and_reload() {
        // Create table
        Spi::run(
            "CREATE TABLE test_drain (model TEXT, id INT, template TEXT, size BIGINT, examples TEXT[], PRIMARY KEY(model, id))",
        )
        .unwrap();

        // Load, feed, dump
        Spi::run(
            "SELECT drain3_load('t1', config := '{\"depth\": 3}', table_name := 'test_drain')",
        )
        .unwrap();
        Spi::run("SELECT drain3_feed('t1', 'User alice logged in')").unwrap();
        Spi::run("SELECT drain3_feed('t1', 'User bob logged in')").unwrap();
        let dumped = Spi::get_one::<i64>("SELECT drain3_dump('t1')").unwrap();
        assert!(dumped.unwrap() >= 1);

        // Verify table data
        let count =
            Spi::get_one::<i64>("SELECT count(*) FROM test_drain WHERE model = 't1'").unwrap();
        assert!(count.unwrap() >= 1);

        let tmpl =
            Spi::get_one::<String>("SELECT template FROM test_drain WHERE model = 't1' LIMIT 1")
                .unwrap();
        assert!(tmpl.unwrap().contains("User"));

        // Unload and reload from table
        Spi::run("SELECT drain3_unload('t1')").unwrap();
        Spi::run(
            "SELECT drain3_load('t1', config := '{\"depth\": 3}', table_name := 'test_drain')",
        )
        .unwrap();

        // Match should still work after reload
        let matched = Spi::get_one::<String>(
            "SELECT drain3_match_model('t1', 'User charlie logged in')",
        )
        .unwrap();
        assert!(matched.is_some());
    }

    #[pg_test]
    fn test_drain3_extract_params_named() {
        let result = Spi::get_one::<pgrx::JsonB>(
            "SELECT drain3_extract_params('User <user> logged in from <ip>', 'User admin logged in from 192.168.1.1')",
        )
        .unwrap()
        .unwrap();

        let obj = result.0.as_object().unwrap();
        // Named keys at top level
        assert_eq!(obj.get("user").unwrap().as_str().unwrap(), "admin");
        assert_eq!(obj.get("ip").unwrap().as_str().unwrap(), "192.168.1.1");
        // Positional values array
        let values = obj.get("_values").unwrap().as_array().unwrap();
        assert_eq!(values[0].as_str().unwrap(), "admin");
        assert_eq!(values[1].as_str().unwrap(), "192.168.1.1");
    }

    #[pg_test]
    fn test_drain3_extract_params_unnamed() {
        let result = Spi::get_one::<pgrx::JsonB>(
            "SELECT drain3_extract_params('User <*> logged in', 'User admin logged in')",
        )
        .unwrap()
        .unwrap();

        let obj = result.0.as_object().unwrap();
        // Only values array, no named keys
        let values = obj.get("_values").unwrap().as_array().unwrap();
        assert_eq!(values[0].as_str().unwrap(), "admin");
        assert!(!obj.contains_key("_1")); // no auto-index keys at top level
    }

    #[pg_test]
    fn test_drain3_models_list() {
        Spi::run("SELECT drain3_load('m1', config := '{\"depth\": 3}')").unwrap();
        Spi::run("SELECT drain3_load('m2', config := '{\"depth\": 4}')").unwrap();

        let count =
            Spi::get_one::<i64>("SELECT count(*) FROM drain3_models()").unwrap();
        assert!(count.unwrap() >= 2);

        Spi::run("SELECT drain3_unload('m1')").unwrap();
        Spi::run("SELECT drain3_unload('m2')").unwrap();
    }
}
