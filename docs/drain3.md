# Drain3 - Log Template Mining

Drain3 is an online log template mining algorithm that clusters similar log messages and extracts templates by replacing variable parts with wildcards (`<*>`).

Based on the paper: *"Drain: An Online Log Parsing Approach with Fixed Depth Tree" (ICWS 2017)*.

## Quick Start

```sql
CREATE EXTENSION pg_wodekit;

-- Create a table for your templates
CREATE TABLE my_drain (
    model TEXT, id INT, template TEXT, size BIGINT, examples TEXT[],
    PRIMARY KEY(model, id)
);

-- Load model into session memory
SELECT drain3_load('syslog', config := '{"depth": 3}', table_name := 'my_drain');

-- Stream feed from a log table (pure in-memory, handles millions of rows)
SELECT drain3_feed('syslog', log_line) FROM app_logs;

-- View discovered templates
SELECT * FROM drain3_model_clusters('syslog');

-- Save to table
SELECT drain3_dump('syslog');

-- Query templates directly with SQL
SELECT template, size, examples FROM my_drain WHERE model = 'syslog' ORDER BY size DESC;
```

## Session-based API (recommended)

Models live in session memory for high-performance streaming. User provides their own table for persistence.

### `drain3_load(name, config, table_name) → bigint`

Load or create a model into session memory. Returns the number of loaded clusters.

```sql
-- New model, pure memory (no table binding)
SELECT drain3_load('syslog', config := '{"depth": 3}');

-- New model, bound to a table for dump
SELECT drain3_load('syslog', config := '{"depth": 3}', table_name := 'my_drain');

-- Reload from existing table data
SELECT drain3_load('syslog', table_name := 'my_drain');
```

### `drain3_feed(name, log_message) → bigint`

Feed a log message into a loaded model. Pure in-memory operation, zero SPI overhead. Returns the matched cluster_id.

```sql
-- Stream millions of rows efficiently
SELECT drain3_feed('syslog', line) FROM huge_logs;
```

### `drain3_match_model(name, log_message) → text`

Match a log message against a loaded model. Returns the template or NULL.

```sql
SELECT drain3_match_model('syslog', 'User admin logged in from 10.0.0.1');
-- Result: 'User <*> logged in from <*>'
```

### `drain3_model_clusters(name) → SETOF (cluster_id, template, size, examples)`

List all clusters in a loaded model, including sample log messages.

```sql
SELECT * FROM drain3_model_clusters('syslog') ORDER BY size DESC;

 cluster_id |          template           | size |              examples
------------+-----------------------------+------+--------------------------------------
          1 | User <*> logged in from <*> |  150 | {"User alice logged in from 192..."}
          4 | Request took <*> ms         |   38 | {"Request took 100 ms", ...}
```

### `drain3_dump(name, table_name) → bigint`

Save model clusters to a table. Uses the table bound at load time, or specify one. Returns the number of clusters saved.

```sql
-- Save to the bound table
SELECT drain3_dump('syslog');

-- Save to a different table
SELECT drain3_dump('syslog', table_name := 'archive_drain');
```

### `drain3_unload(name) → bool`

Free a model from session memory.

### `drain3_models() → SETOF (name, clusters, table_name)`

List all models loaded in the current session.

## Table Schema

User creates their own table. Required columns:

```sql
CREATE TABLE my_drain (
    model    TEXT,       -- model name
    id       INT,        -- cluster id
    template TEXT,       -- e.g. 'User <*> logged in from <*>'
    size     BIGINT,     -- number of matched messages
    examples TEXT[],     -- sample log messages (up to 5, deduped)
    PRIMARY KEY (model, id)
);
```

No opaque blobs — all data is plain SQL-queryable.

## Parameter Extraction

Supports both unnamed (`<*>`) and named (`<user>`, `<ip>`) wildcards. Returns JSONB.

```sql
-- Named wildcards (user renames <*> after mining)
SELECT drain3_extract_params(
    'User <user> logged in from <ip>',
    'User admin logged in from 192.168.1.1'
);
-- {"user": "admin", "ip": "192.168.1.1"}

-- Unnamed wildcards get auto-indexed
SELECT drain3_extract_params(
    'User <*> logged in from <*>',
    'User admin logged in from 192.168.1.1'
);
-- {"_1": "admin", "_2": "192.168.1.1"}
```

## Value-based API

For one-off use without session management. Models are passed as `pgdrainstate` values.

```sql
-- Aggregate mine
SELECT drain3_mine(log_line) AS model FROM logs;

-- Match against a value
SELECT drain3_match(model, 'User admin logged in');

-- List clusters from a value
SELECT * FROM drain3_clusters(model);
```

## Configuration

All fields are optional with sensible defaults. Pass as JSONB to `drain3_load`.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `depth` | 4 | Prefix tree depth (min 3). Lower = more aggressive merging. |
| `sim_th` | 0.4 | Similarity threshold [0,1]. Higher = stricter, more clusters. |
| `max_children` | 100 | Max children per tree node. Exceeded → wildcard routing. |
| `max_clusters` | 1024 | Max clusters. Oldest evicted when exceeded. |
| `param_str` | `<*>` | Wildcard marker string. |
| `parametrize_numeric_tokens` | true | Auto-replace pure numeric tokens with wildcard. |
| `extra_delimiters` | `[]` | Extra split characters in addition to whitespace. |
| `delimiters` | `[]` | Override: split ONLY on these characters (replaces whitespace). |

### Tuning Guide

- **Too many clusters?** Lower `sim_th` or `depth`
- **Too few clusters?** Raise `sim_th` or `depth`
- **Key-value logs** (`key=value`): `"extra_delimiters": ["="]`
- **Pipe-delimited** (`a|b|c`): `"delimiters": ["|"]`
- **Short logs** (3-5 tokens): `"depth": 3`
- **Long logs** (10+ tokens): `"depth": 4` or higher

## Algorithm

1. **Tokenize**: Split on whitespace (or custom delimiters), parametrize numeric tokens
2. **Tree search**: Navigate prefix tree by token count → first N tokens
3. **Fast match**: Compare candidates by token similarity ratio
4. **Update**: If similarity >= threshold, merge template (differing tokens → `<*>`)
5. **Create**: Otherwise, create a new cluster with the message as first example

## Examples

### Full Workflow

```sql
-- Setup
CREATE TABLE drain_templates (
    model TEXT, id INT, template TEXT, size BIGINT, examples TEXT[],
    PRIMARY KEY(model, id)
);

-- Load and train
SELECT drain3_load('app', config := '{"depth": 3}', table_name := 'drain_templates');
SELECT drain3_feed('app', message) FROM app_logs WHERE ts > now() - interval '1 day';
SELECT drain3_dump('app');

-- Next day: reload and continue training
SELECT drain3_load('app', table_name := 'drain_templates');
SELECT drain3_feed('app', message) FROM app_logs WHERE ts > now() - interval '1 day';
SELECT drain3_dump('app');

-- Classify new logs
SELECT drain3_match_model('app', message) AS template, count(*)
FROM new_logs GROUP BY 1 ORDER BY 2 DESC;

-- Rename wildcards for structured extraction
UPDATE drain_templates
SET template = 'User <user> logged in from <ip>'
WHERE template = 'User <*> logged in from <*>';

-- Extract structured params
SELECT drain3_extract_params(t.template, l.message) AS params
FROM new_logs l
JOIN drain_templates t ON drain3_match_model('app', l.message) = t.template;
```

### With FDW (Doris/MySQL)

```sql
SELECT drain3_load('doris', config := '{"depth": 3}', table_name := 'drain_templates');
SELECT drain3_feed('doris', log_line) FROM doris_audit_log;
SELECT drain3_dump('doris');

-- Top log patterns
SELECT template, size FROM drain_templates WHERE model = 'doris' ORDER BY size DESC LIMIT 10;
```
