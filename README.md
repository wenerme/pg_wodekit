# pg_wodekit

PostgreSQL utility functions toolkit, built with [pgrx](https://github.com/pgcentralfoundation/pgrx).

## Modules

### drain3 — Log Template Mining

Online log template mining via the [Drain3 algorithm](https://github.com/logpai/Drain3). Clusters similar log messages and extracts templates by replacing variable parts with wildcards.

```sql
CREATE EXTENSION pg_wodekit;

-- Create your template storage table
CREATE TABLE my_drain (
    model TEXT, id INT, template TEXT, size BIGINT, examples TEXT[],
    PRIMARY KEY(model, id)
);

-- Load model into session memory
SELECT drain3_load('syslog', config := '{"depth": 3}', table_name := 'my_drain');

-- Stream feed from a log table (handles millions of rows efficiently)
SELECT drain3_feed('syslog', log_line) FROM app_logs;

-- View discovered templates
SELECT * FROM drain3_model_clusters('syslog') ORDER BY size DESC;

-- Save to table
SELECT drain3_dump('syslog');

-- Match new logs
SELECT drain3_match_model('syslog', 'User admin logged in from 10.0.0.1');
-- → 'User <*> logged in from <*>'

-- Extract parameters (named wildcards supported)
SELECT drain3_extract_params('User <user> logged in from <ip>', msg);
-- → {"_values": ["admin", "192.168.1.1"], "user": "admin", "ip": "192.168.1.1"}
```

See [docs/drain3.md](docs/drain3.md) for full documentation.

## Installation

Requires PostgreSQL 14–18 and Rust 1.88.0+.

```bash
cargo install --locked cargo-pgrx --version 0.16.1
cargo pgrx install --pg-config $(pg_config)
```

Then in PostgreSQL:

```sql
CREATE EXTENSION pg_wodekit;
```

## Development

```bash
cargo build --features pg16
USER=admin cargo test pg_test
RUSTFLAGS="-D warnings" cargo clippy --features pg16 --tests --no-deps
```

## License

MIT
