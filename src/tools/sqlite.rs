//! SQLite tools, ported from the `database_query`/`database_execute` tools in `src/tools/index.ts`.

use super::Tool;
use crate::types::{RiskLevel, ToolDeclaration};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

fn declaration_once(name: &str, description: &str, parameters: serde_json::Value) -> ToolDeclaration {
    ToolDeclaration { name: name.to_string(), description: description.to_string(), parameters }
}

use std::collections::HashMap;
use std::sync::Mutex;

pub struct DatabaseQueryTool {
    pub cwd: PathBuf,
}

#[async_trait]
impl Tool for DatabaseQueryTool {
     fn declaration(&self) -> &ToolDeclaration {
        let owned = declaration_once("database_query", "Run a read-only SQLite SELECT or EXPLAIN query against a project database.", json!({"type": "object", "properties": {"path": {"type": "string"}, "sql": {"type": "string"}}, "required": ["path", "sql"], "additionalProperties": false}));
        Box::leak(Box::new(owned))
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
        let sql = args.get("sql").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if !{
            let lowered = sql.to_lowercase();
            lowered.starts_with("select ") || lowered.starts_with("explain ")
        } {
            bail!("database_query accepts only SELECT or EXPLAIN");
        }
        let file = super::within(&self.cwd, &path)?;
        if !file.is_file() {
            bail!("Database file does not exist: {path}");
        }
        // rusqlite calls block; run on the blocking pool.
        let file = file.clone();
        let rows = tokio::task::spawn_blocking(move || query_rows(&file, &sql)).await??;
        Ok(super::clip(&serde_json::to_string_pretty(&rows)?, super::OUTPUT_LIMIT))
    }
}

pub struct DatabaseExecuteTool {
    pub cwd: PathBuf,
}

#[async_trait]
impl Tool for DatabaseExecuteTool {
    fn declaration(&self) -> &ToolDeclaration {
        Box::leak(Box::new(declaration_once("database_execute", "Execute one SQLite statement against a project database; requires write approval.", json!({"type": "object", "properties": {"path": {"type": "string"}, "sql": {"type": "string"}}, "required": ["path", "sql"], "additionalProperties": false}))))
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Write
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
        let sql = args.get("sql").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if sql.is_empty() {
            bail!("database_execute requires SQL");
        }
        let file = super::within(&self.cwd, &path)?;
        let file = file.clone();
        tokio::task::spawn_blocking(move || execute_statement(&file, &sql)).await??;
        Ok("SQLite statement executed".to_string())
    }
}

fn json_value(value: rusqlite::types::Value) -> Value {
    match value {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(value) => json!(value),
        rusqlite::types::Value::Real(value) => json!(value),
        rusqlite::types::Value::Text(value) => json!(value),
        rusqlite::types::Value::Blob(value) => json!(format!("{} bytes blob", value.len())),
    }
}

fn query_rows(file: &PathBuf, sql: &str) -> Result<Value> {
    let database = rusqlite::Connection::open(file).map_err(|error| anyhow!("{error}"))?;
    let mut statement = database.prepare(sql).map_err(|error| anyhow!("{error}"))?;
    let columns: Vec<String> = statement.column_names().iter().map(|name| name.to_string()).collect();
    let rows = statement
        .query_map([], |row| {
            let mut object = serde_json::Map::new();
            for (index, column) in columns.iter().enumerate() {
                let column_value: rusqlite::types::Value = row.get(index)?;
                object.insert(column.clone(), json_value(column_value));
            }
            Ok(Value::Object(object))
        })
        .map_err(|error| anyhow!("{error}"))?;
    let mut output = Vec::new();
    for row in rows {
        output.push(row.map_err(|error| anyhow!("{error}"))?);
    }
    Ok(Value::Array(output))
}

fn execute_statement(file: &PathBuf, sql: &str) -> Result<()> {
    let database = rusqlite::Connection::open(file).map_err(|error| anyhow!("{error}"))?;
    database.execute_batch(sql).map_err(|error| anyhow!("{error}"))?;
    Ok(())
}
