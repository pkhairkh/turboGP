//! # DDL parser: CREATE TABLE / DROP TABLE (Wave 3).
//!
//! Parses a subset of SQL DDL sufficient for creating tables with typed
//! columns, NULL/NOT NULL constraints, DEFAULT literals, PRIMARY KEY,
//! and REFERENCES (single-column foreign keys).
//!
//! Supported types: INT, INTEGER, BIGINT, SMALLINT, TINYINT, VARCHAR(n),
//! NVARCHAR(n), TEXT, FLOAT, REAL, DECIMAL(p,s), NUMERIC(p,s), BIT,
//! BOOLEAN, DATE, TIMESTAMP, DATETIME2.

use crate::sql::lexer::{tokenize, Token};

/// SQL column type.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnType {
    /// 32-bit integer (stored as u64 cell).
    Int,
    /// 64-bit integer.
    BigInt,
    /// 16-bit integer.
    SmallInt,
    /// 8-bit integer.
    TinyInt,
    /// Variable-length string. The optional length is advisory only —
    /// turboGP stores all strings in a sidecar heap, so the length is
    /// not enforced at the cell level.
    Varchar(Option<usize>),
    /// NVARCHAR — same storage as Varchar.
    Nvarchar(Option<usize>),
    /// TEXT — alias for VARCHAR(MAX).
    Text,
    /// 64-bit IEEE float.
    Float,
    /// 32-bit IEEE float (stored as f64).
    Real,
    /// Fixed-point decimal with precision and scale.
    Decimal(Option<u32>, Option<u32>),
    /// NUMERIC — alias for DECIMAL.
    Numeric(Option<u32>, Option<u32>),
    /// Boolean (stored as 0/1 u64).
    Boolean,
    /// Bit (stored as 0/1 u64).
    Bit,
    /// DATE (stored as days-since-epoch u64).
    Date,
    /// TIMESTAMP / DATETIME2 (stored as u64).
    Timestamp,
    /// JSON type (Wave 70). Stored as a VARCHAR sidecar — the JSON string
    /// lives in the string_columns sidecar, and the u64 cell holds the
    /// xxh3 hash. JSON_VALUE / JSON_QUERY operate on the string sidecar.
    Json,
    /// ARRAY type (Wave 70). Stored as a VARCHAR sidecar containing a
    /// JSON-encoded array (e.g. `[1, 2, 3]`). The u64 cell holds the hash.
    Array(Box<ColumnType>),
    /// UUID type (Wave 70). Stored as a 128-bit value across two u64
    /// cells (high 64 bits + low 64 bits). For simplicity, the current
    /// implementation stores only the low 64 bits in a single cell.
    Uuid,
    /// BYTEA type (Wave 70). Stored as a VARCHAR sidecar containing
    /// base64-encoded bytes. The u64 cell holds the hash.
    Bytea,
    /// ENUM type (Wave 70). The Vec contains the allowed enum values.
    /// Stored as a u64 index into the values Vec.
    Enum(Vec<String>),
}

impl ColumnType {
    /// Returns the type name as it would appear in a CREATE TABLE.
    pub fn type_name(&self) -> &'static str {
        match self {
            ColumnType::Int | ColumnType::SmallInt | ColumnType::TinyInt => "INT",
            ColumnType::BigInt => "BIGINT",
            ColumnType::Varchar(_) => "VARCHAR",
            ColumnType::Nvarchar(_) => "NVARCHAR",
            ColumnType::Text => "TEXT",
            ColumnType::Float => "FLOAT",
            ColumnType::Real => "REAL",
            ColumnType::Decimal(_, _) => "DECIMAL",
            ColumnType::Numeric(_, _) => "NUMERIC",
            ColumnType::Boolean => "BOOLEAN",
            ColumnType::Bit => "BIT",
            ColumnType::Date => "DATE",
            ColumnType::Timestamp => "TIMESTAMP",
            ColumnType::Json => "JSON",
            ColumnType::Array(_) => "ARRAY",
            ColumnType::Uuid => "UUID",
            ColumnType::Bytea => "BYTEA",
            ColumnType::Enum(_) => "ENUM",
        }
    }
}

/// A column definition in a CREATE TABLE statement.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// Column name.
    pub name: String,
    /// Column type.
    pub col_type: ColumnType,
    /// True if NOT NULL was specified.
    pub not_null: bool,
    /// True if PRIMARY KEY was specified (implies NOT NULL).
    pub primary_key: bool,
    /// Optional DEFAULT literal value (stored as a string for simplicity;
    /// the executor interprets it based on col_type).
    pub default: Option<String>,
    /// True if IDENTITY(1,1) was specified (auto-increment).
    pub identity: bool,
    /// Optional REFERENCES clause: (referenced_table, referenced_column).
    pub references: Option<(String, String)>,
}

/// A parsed CREATE TABLE statement.
#[derive(Debug, Clone)]
pub struct CreateTable {
    /// Optional schema name (e.g. "HR" in "HR.Employees"). Defaults to "dbo".
    pub schema: String,
    /// Table name.
    pub name: String,
    /// Column definitions, in order.
    pub columns: Vec<ColumnDef>,
    /// True if IF NOT EXISTS was specified.
    pub if_not_exists: bool,
}

/// A parsed DROP TABLE statement.
#[derive(Debug, Clone)]
pub struct DropTable {
    pub schema: String,
    pub name: String,
    pub if_exists: bool,
}

/// The result of parsing a DDL statement.
#[derive(Debug, Clone)]
pub enum DdlStatement {
    Create(CreateTable),
    Drop(DropTable),
    /// CREATE SCHEMA — accepted but a no-op (schemas are implicit).
    CreateSchema(String),
    /// ALTER TABLE — Wave 66. Adds, drops, or retypes a column.
    AlterTable(AlterTable),
    /// CREATE INDEX — Wave 66.
    CreateIndex(CreateIndex),
    /// DROP INDEX — Wave 66.
    DropIndex(DropIndex),
}

/// ALTER TABLE action (Wave 66).
#[derive(Debug, Clone)]
pub enum AlterAction {
    /// `ADD COLUMN col TYPE [DEFAULT x]`
    AddColumn(ColumnDef),
    /// `DROP COLUMN col`
    DropColumn(String),
    /// `ALTER COLUMN col TYPE new_type`
    AlterColumnType {
        /// Column name.
        column: String,
        /// New type.
        new_type: ColumnType,
    },
}

/// A parsed ALTER TABLE statement (Wave 66).
#[derive(Debug, Clone)]
pub struct AlterTable {
    /// Schema (defaults to "dbo").
    pub schema: String,
    /// Table name.
    pub name: String,
    /// The action to perform.
    pub action: AlterAction,
}

/// A parsed CREATE INDEX statement (Wave 66).
#[derive(Debug, Clone)]
pub struct CreateIndex {
    /// Index name.
    pub index_name: String,
    /// Table to index.
    pub table: String,
    /// Column to index.
    pub column: String,
    /// True if `IF NOT EXISTS` was specified.
    pub if_not_exists: bool,
}

/// A parsed DROP INDEX statement (Wave 66).
#[derive(Debug, Clone)]
pub struct DropIndex {
    /// Index name.
    pub index_name: String,
    /// True if `IF EXISTS` was specified.
    pub if_exists: bool,
}

/// Parse a DDL string. Returns None if the string is not a DDL statement
/// (i.e. it's a SELECT or other DML).
pub fn parse_ddl(sql: &str) -> Result<Option<DdlStatement>, String> {
    let tokens = tokenize(sql)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    let first = match &tokens[0] {
        Token::Keyword(k) => k.as_str(),
        _ => return Ok(None),
    };
    match first {
        "CREATE" => parse_create(&tokens[1..]).map(Some),
        "DROP" => parse_drop(&tokens[1..]).map(Some),
        "ALTER" => parse_alter(&tokens[1..]).map(Some),
        _ => Ok(None),
    }
}

fn parse_create(tokens: &[Token]) -> Result<DdlStatement, String> {
    if tokens.is_empty() {
        return Err("expected TABLE, SCHEMA, or INDEX after CREATE".into());
    }
    match &tokens[0] {
        Token::Keyword(k) if k == "TABLE" => {
            let ct = parse_create_table(&tokens[1..])?;
            Ok(DdlStatement::Create(ct))
        }
        Token::Keyword(k) if k == "SCHEMA" => {
            // CREATE SCHEMA name — just extract the name.
            if tokens.len() < 2 {
                return Err("expected schema name after CREATE SCHEMA".into());
            }
            let name = match &tokens[1] {
                Token::Ident(s) => s.clone(),
                _ => return Err("expected schema name".into()),
            };
            Ok(DdlStatement::CreateSchema(name))
        }
        Token::Keyword(k) if k == "INDEX" => {
            let ci = parse_create_index(&tokens[1..])?;
            Ok(DdlStatement::CreateIndex(ci))
        }
        other => Err(format!("expected TABLE, SCHEMA, or INDEX after CREATE, got {other:?}")),
    }
}

fn parse_drop(tokens: &[Token]) -> Result<DdlStatement, String> {
    if tokens.is_empty() {
        return Err("expected TABLE or INDEX after DROP".into());
    }
    match &tokens[0] {
        Token::Keyword(k) if k == "TABLE" => parse_drop_table(&tokens[1..]).map(DdlStatement::Drop),
        Token::Keyword(k) if k == "INDEX" => {
            parse_drop_index(&tokens[1..]).map(DdlStatement::DropIndex)
        }
        other => Err(format!("expected TABLE or INDEX after DROP, got {other:?}")),
    }
}

fn parse_alter(tokens: &[Token]) -> Result<DdlStatement, String> {
    // Expect TABLE
    if tokens.is_empty() {
        return Err("expected TABLE after ALTER".into());
    }
    match &tokens[0] {
        Token::Keyword(k) if k == "TABLE" => {
            let at = parse_alter_table(&tokens[1..])?;
            Ok(DdlStatement::AlterTable(at))
        }
        other => Err(format!("expected TABLE after ALTER, got {other:?}")),
    }
}

fn parse_alter_table(tokens: &[Token]) -> Result<AlterTable, String> {
    let mut pos = 0;
    let (schema, name) = parse_qualified_name(tokens, &mut pos)?;
    if pos >= tokens.len() {
        return Err("expected ADD / DROP / ALTER after table name".into());
    }
    let action = match &tokens[pos] {
        Token::Keyword(k) if k == "ADD" => {
            pos += 1;
            // Optional COLUMN keyword
            if pos < tokens.len() {
                if let Token::Keyword(k) = &tokens[pos] {
                    if k == "COLUMN" {
                        pos += 1;
                    }
                }
            }
            // Parse a single column definition.
            let col = parse_column_def(tokens, &mut pos)?;
            AlterAction::AddColumn(col)
        }
        Token::Keyword(k) if k == "DROP" => {
            pos += 1;
            if pos < tokens.len() {
                if let Token::Keyword(k) = &tokens[pos] {
                    if k == "COLUMN" {
                        pos += 1;
                    }
                }
            }
            if pos >= tokens.len() {
                return Err("expected column name after DROP COLUMN".into());
            }
            let col_name = match &tokens[pos] {
                Token::Ident(s) => s.clone(),
                other => return Err(format!("expected column name, got {other:?}")),
            };
            AlterAction::DropColumn(col_name)
        }
        Token::Keyword(k) if k == "ALTER" => {
            pos += 1;
            if pos < tokens.len() {
                if let Token::Keyword(k) = &tokens[pos] {
                    if k == "COLUMN" {
                        pos += 1;
                    }
                }
            }
            if pos >= tokens.len() {
                return Err("expected column name after ALTER COLUMN".into());
            }
            let col_name = match &tokens[pos] {
                Token::Ident(s) => s.clone(),
                other => return Err(format!("expected column name, got {other:?}")),
            };
            pos += 1;
            // Expect TYPE
            if pos >= tokens.len() {
                return Err("expected TYPE after column name".into());
            }
            match &tokens[pos] {
                Token::Keyword(k) if k == "TYPE" => pos += 1,
                other => return Err(format!("expected TYPE, got {other:?}")),
            }
            let new_type = parse_type(tokens, &mut pos)?;
            AlterAction::AlterColumnType { column: col_name, new_type }
        }
        other => return Err(format!("expected ADD / DROP / ALTER, got {other:?}")),
    };
    Ok(AlterTable { schema, name, action })
}

fn parse_create_index(tokens: &[Token]) -> Result<CreateIndex, String> {
    let mut pos = 0;
    // Optional IF NOT EXISTS
    let mut if_not_exists = false;
    if pos < tokens.len() {
        if let Token::Keyword(k) = &tokens[pos] {
            if k == "IF" {
                pos += 1;
                if pos >= tokens.len() {
                    return Err("expected NOT after IF".into());
                }
                match &tokens[pos] {
                    Token::Keyword(k) if k == "NOT" => pos += 1,
                    other => return Err(format!("expected NOT, got {other:?}")),
                }
                if pos >= tokens.len() {
                    return Err("expected EXISTS after NOT".into());
                }
                match &tokens[pos] {
                    Token::Keyword(k) if k == "EXISTS" => pos += 1,
                    other => return Err(format!("expected EXISTS, got {other:?}")),
                }
                if_not_exists = true;
            }
        }
    }
    // Index name
    if pos >= tokens.len() {
        return Err("expected index name after CREATE INDEX".into());
    }
    let index_name = match &tokens[pos] {
        Token::Ident(s) => s.clone(),
        other => return Err(format!("expected index name, got {other:?}")),
    };
    pos += 1;
    // Expect ON
    if pos >= tokens.len() {
        return Err("expected ON after index name".into());
    }
    match &tokens[pos] {
        Token::Keyword(k) if k == "ON" => pos += 1,
        other => return Err(format!("expected ON, got {other:?}")),
    }
    // Table name
    let (_table_schema, table) = parse_qualified_name(tokens, &mut pos)?;
    // Expect ( column )
    if pos >= tokens.len() {
        return Err("expected ( after table name".into());
    }
    match &tokens[pos] {
        Token::LParen => pos += 1,
        other => return Err(format!("expected (, got {other:?}")),
    }
    if pos >= tokens.len() {
        return Err("expected column name".into());
    }
    let column = match &tokens[pos] {
        Token::Ident(s) => s.clone(),
        other => return Err(format!("expected column name, got {other:?}")),
    };
    pos += 1;
    if pos >= tokens.len() {
        return Err("expected ) after column name".into());
    }
    match &tokens[pos] {
        Token::RParen => pos += 1,
        other => return Err(format!("expected ), got {other:?}")),
    }
    // Trailing tokens (e.g. USING btree, method_opt) are ignored.
    Ok(CreateIndex { index_name, table, column, if_not_exists })
}

fn parse_drop_index(tokens: &[Token]) -> Result<DropIndex, String> {
    let mut pos = 0;
    let mut if_exists = false;
    if pos < tokens.len() {
        if let Token::Keyword(k) = &tokens[pos] {
            if k == "IF" {
                pos += 1;
                if pos >= tokens.len() {
                    return Err("expected EXISTS after IF".into());
                }
                match &tokens[pos] {
                    Token::Keyword(k) if k == "EXISTS" => pos += 1,
                    other => return Err(format!("expected EXISTS, got {other:?}")),
                }
                if_exists = true;
            }
        }
    }
    if pos >= tokens.len() {
        return Err("expected index name after DROP INDEX".into());
    }
    let index_name = match &tokens[pos] {
        Token::Ident(s) => s.clone(),
        Token::Keyword(k) => k.clone(),
        other => return Err(format!("expected index name, got {other:?}")),
    };
    Ok(DropIndex { index_name, if_exists })
}

fn parse_drop_table(tokens: &[Token]) -> Result<DropTable, String> {
    let mut pos = 0;

    // Optional IF EXISTS
    let mut if_exists = false;
    if pos < tokens.len() {
        if let Token::Keyword(k) = &tokens[pos] {
            if k == "IF" {
                pos += 1;
                if pos >= tokens.len() {
                    return Err("expected EXISTS after IF".into());
                }
                match &tokens[pos] {
                    Token::Keyword(k) if k == "EXISTS" => pos += 1,
                    _ => return Err("expected EXISTS after IF".into()),
                }
                if_exists = true;
            }
        }
    }

    let (schema, name) = parse_qualified_name(tokens, &mut pos)?;
    Ok(DropTable { schema, name, if_exists })
}

fn parse_create_table(tokens: &[Token]) -> Result<CreateTable, String> {
    let mut pos = 0;

    // Optional IF NOT EXISTS
    let mut if_not_exists = false;
    if pos < tokens.len() {
        if let Token::Keyword(k) = &tokens[pos] {
            if k == "IF" {
                pos += 1;
                if pos >= tokens.len() {
                    return Err("expected NOT after IF".into());
                }
                match &tokens[pos] {
                    Token::Keyword(k) if k == "NOT" => pos += 1,
                    _ => return Err("expected NOT after IF".into()),
                }
                if pos >= tokens.len() {
                    return Err("expected EXISTS after NOT".into());
                }
                match &tokens[pos] {
                    Token::Keyword(k) if k == "EXISTS" => pos += 1,
                    _ => return Err("expected EXISTS after NOT".into()),
                }
                if_not_exists = true;
            }
        }
    }

    // Table name: [schema.]name
    let (schema, name) = parse_qualified_name(tokens, &mut pos)?;

    // Expect (
    if pos >= tokens.len() {
        return Err("expected ( after table name".into());
    }
    match &tokens[pos] {
        Token::LParen => pos += 1,
        other => return Err(format!("expected ( after table name, got {other:?}")),
    }

    // Parse column definitions
    let mut columns = Vec::new();
    loop {
        if pos >= tokens.len() {
            return Err("unterminated column list".into());
        }
        // Check for closing )
        if let Token::RParen = &tokens[pos] {
            pos += 1;
            break;
        }

        // Parse one column def
        let col = parse_column_def(tokens, &mut pos)?;
        columns.push(col);

        // Expect , or )
        if pos >= tokens.len() {
            return Err("unterminated column list".into());
        }
        match &tokens[pos] {
            Token::Comma => pos += 1,
            Token::RParen => {
                pos += 1;
                break;
            }
            other => return Err(format!("expected , or ) in column list, got {other:?}")),
        }
    }

    // Ignore trailing tokens (e.g. table-level constraints like ENGINE=...)
    Ok(CreateTable { schema, name, columns, if_not_exists })
}

fn parse_qualified_name(tokens: &[Token], pos: &mut usize) -> Result<(String, String), String> {
    if *pos >= tokens.len() {
        return Err("expected table name".into());
    }
    let first = match &tokens[*pos] {
        Token::Ident(s) => s.clone(),
        Token::Keyword(k) => k.clone(), // allow keywords as names (e.g. "TABLE")
        other => return Err(format!("expected identifier, got {other:?}")),
    };
    *pos += 1;

    // Check for schema.name
    if *pos < tokens.len() {
        if let Token::Op(op) = &tokens[*pos] {
            if op == "." {
                *pos += 1;
                if *pos >= tokens.len() {
                    return Err("expected name after .".into());
                }
                let name = match &tokens[*pos] {
                    Token::Ident(s) => s.clone(),
                    Token::Keyword(k) => k.clone(),
                    other => return Err(format!("expected name after ., got {other:?}")),
                };
                *pos += 1;
                return Ok((first, name));
            }
        }
    }

    Ok(("dbo".into(), first))
}

fn parse_column_def(tokens: &[Token], pos: &mut usize) -> Result<ColumnDef, String> {
    // Column name
    if *pos >= tokens.len() {
        return Err("expected column name".into());
    }
    let name = match &tokens[*pos] {
        Token::Ident(s) => s.clone(),
        other => return Err(format!("expected column name, got {other:?}")),
    };
    *pos += 1;

    // Column type
    let col_type = parse_type(tokens, pos)?;

    // Constraints
    let mut not_null = false;
    let mut primary_key = false;
    let mut default = None;
    let mut identity = false;
    let mut references = None;

    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Keyword(k) if k == "NOT" => {
                *pos += 1;
                if *pos >= tokens.len() {
                    return Err("expected NULL after NOT".into());
                }
                match &tokens[*pos] {
                    Token::Keyword(k) if k == "NULL" => {
                        not_null = true;
                        *pos += 1;
                    }
                    other => return Err(format!("expected NULL after NOT, got {other:?}")),
                }
            }
            Token::Keyword(k) if k == "NULL" => {
                *pos += 1; // explicit NULL allowed
            }
            Token::Keyword(k) if k == "PRIMARY" => {
                *pos += 1;
                if *pos >= tokens.len() {
                    return Err("expected KEY after PRIMARY".into());
                }
                match &tokens[*pos] {
                    Token::Keyword(k) if k == "KEY" => {
                        primary_key = true;
                        not_null = true;
                        *pos += 1;
                    }
                    other => return Err(format!("expected KEY after PRIMARY, got {other:?}")),
                }
            }
            Token::Keyword(k) if k == "DEFAULT" => {
                *pos += 1;
                if *pos >= tokens.len() {
                    return Err("expected value after DEFAULT".into());
                }
                default = Some(token_to_literal(&tokens[*pos])?);
                *pos += 1;
            }
            Token::Keyword(k) if k == "IDENTITY" => {
                *pos += 1;
                // Optional (1,1)
                if *pos < tokens.len() {
                    if let Token::LParen = &tokens[*pos] {
                        *pos += 1;
                        // Skip until matching )
                        while *pos < tokens.len() {
                            match &tokens[*pos] {
                                Token::RParen => {
                                    *pos += 1;
                                    break;
                                }
                                _ => *pos += 1,
                            }
                        }
                    }
                }
                identity = true;
            }
            Token::Keyword(k) if k == "REFERENCES" => {
                *pos += 1;
                let (ref_schema, ref_table) = parse_qualified_name(tokens, pos)?;
                let _ = ref_schema;
                // Optional (column)
                let ref_col = if *pos < tokens.len() {
                    if let Token::LParen = &tokens[*pos] {
                        *pos += 1;
                        if *pos >= tokens.len() {
                            return Err("expected column after REFERENCES (".into());
                        }
                        let c = match &tokens[*pos] {
                            Token::Ident(s) => s.clone(),
                            other => return Err(format!("expected column, got {other:?}")),
                        };
                        *pos += 1;
                        if *pos >= tokens.len() {
                            return Err("expected ) after REFERENCES column".into());
                        }
                        match &tokens[*pos] {
                            Token::RParen => *pos += 1,
                            other => return Err(format!("expected ), got {other:?}")),
                        }
                        c
                    } else {
                        "id".into() // default
                    }
                } else {
                    "id".into()
                };
                references = Some((ref_table, ref_col));
            }
            Token::Comma | Token::RParen => break,
            _ => {
                // Skip unknown constraint tokens (e.g. COLLATE, CHECK, etc.)
                *pos += 1;
            }
        }
    }

    Ok(ColumnDef { name, col_type, not_null, primary_key, default, identity, references })
}

fn parse_type(tokens: &[Token], pos: &mut usize) -> Result<ColumnType, String> {
    if *pos >= tokens.len() {
        return Err("expected column type".into());
    }
    let type_name = match &tokens[*pos] {
        Token::Keyword(k) => k.clone(),
        Token::Ident(s) => s.to_uppercase(),
        other => return Err(format!("expected type, got {other:?}")),
    };
    *pos += 1;

    // Optional (length) or (precision, scale)
    let mut len1: Option<u32> = None;
    let mut len2: Option<u32> = None;
    if *pos < tokens.len() {
        if let Token::LParen = &tokens[*pos] {
            *pos += 1;
            if *pos >= tokens.len() {
                return Err("expected number after (".into());
            }
            len1 = match &tokens[*pos] {
                Token::Int(n) => Some(*n as u32),
                other => return Err(format!("expected number, got {other:?}")),
            };
            *pos += 1;
            if *pos < tokens.len() {
                if let Token::Comma = &tokens[*pos] {
                    *pos += 1;
                    if *pos >= tokens.len() {
                        return Err("expected scale after ,".into());
                    }
                    len2 = match &tokens[*pos] {
                        Token::Int(n) => Some(*n as u32),
                        other => return Err(format!("expected scale, got {other:?}")),
                    };
                    *pos += 1;
                }
            }
            if *pos >= tokens.len() {
                return Err("expected ) after type params".into());
            }
            match &tokens[*pos] {
                Token::RParen => *pos += 1,
                other => return Err(format!("expected ), got {other:?}")),
            }
        }
    }

    match type_name.as_str() {
        "INT" | "INTEGER" => Ok(ColumnType::Int),
        "BIGINT" => Ok(ColumnType::BigInt),
        "SMALLINT" => Ok(ColumnType::SmallInt),
        "TINYINT" => Ok(ColumnType::TinyInt),
        "VARCHAR" => Ok(ColumnType::Varchar(len1.map(|v| v as usize))),
        "NVARCHAR" => Ok(ColumnType::Nvarchar(len1.map(|v| v as usize))),
        "TEXT" => Ok(ColumnType::Text),
        "FLOAT" | "DOUBLE" => Ok(ColumnType::Float),
        "REAL" => Ok(ColumnType::Real),
        "DECIMAL" => Ok(ColumnType::Decimal(len1, len2)),
        "NUMERIC" => Ok(ColumnType::Numeric(len1, len2)),
        "BIT" => Ok(ColumnType::Bit),
        "BOOLEAN" | "BOOL" => Ok(ColumnType::Boolean),
        "DATE" => Ok(ColumnType::Date),
        "TIMESTAMP" | "DATETIME2" | "DATETIME" => Ok(ColumnType::Timestamp),
        "JSON" => Ok(ColumnType::Json),
        "UUID" => Ok(ColumnType::Uuid),
        "BYTEA" | "BINARY" | "VARBINARY" => Ok(ColumnType::Bytea),
        "ARRAY" => Ok(ColumnType::Array(Box::new(ColumnType::Text))),
        "ENUM" => {
            // ENUM('val1', 'val2', ...) — parse the value list from the parenthesized args.
            let mut values = Vec::new();
            // The parse logic above already consumed the parenthesized args as len1/len2.
            // For ENUM, we need to re-parse the tokens. This is a simplification —
            // a full implementation would parse the string literals in the parens.
            // For now, return an empty enum (values must be added via ALTER TABLE).
            // TODO: parse ENUM('a', 'b', 'c') properly.
            values.push("placeholder".to_string());
            Ok(ColumnType::Enum(values))
        }
        other => Err(format!("unknown type: {other}")),
    }
}

fn token_to_literal(tok: &Token) -> Result<String, String> {
    match tok {
        Token::Int(n) => Ok(n.to_string()),
        Token::Float(f) => Ok(f.to_string()),
        Token::String(s) => Ok(format!("'{s}'")),
        Token::Keyword(k) if k == "NULL" => Ok("NULL".into()),
        other => Err(format!("unsupported default value: {other:?}")),
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_create() {
        let sql = "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(100) NOT NULL)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.schema, "dbo");
                assert_eq!(ct.name, "users");
                assert_eq!(ct.columns.len(), 2);
                assert_eq!(ct.columns[0].name, "id");
                assert!(ct.columns[0].primary_key);
                assert!(ct.columns[0].not_null);
                assert_eq!(ct.columns[1].name, "name");
                assert!(ct.columns[1].not_null);
                assert!(!ct.columns[1].primary_key);
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_qualified_name() {
        let sql = "CREATE TABLE HR.Employees (id INT)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.schema, "HR");
                assert_eq!(ct.name, "Employees");
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_all_types() {
        let sql = "CREATE TABLE t (
            a INT, b BIGINT, c SMALLINT, d TINYINT,
            e VARCHAR(50), f NVARCHAR(100), g TEXT,
            h FLOAT, i REAL, j DECIMAL(18,2), k NUMERIC(10,4),
            l BIT, m BOOLEAN, n DATE, o TIMESTAMP
        )";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.columns.len(), 15);
                assert_eq!(ct.columns[0].col_type, ColumnType::Int);
                assert_eq!(ct.columns[1].col_type, ColumnType::BigInt);
                assert_eq!(ct.columns[2].col_type, ColumnType::SmallInt);
                assert_eq!(ct.columns[3].col_type, ColumnType::TinyInt);
                assert_eq!(ct.columns[4].col_type, ColumnType::Varchar(Some(50)));
                assert_eq!(ct.columns[5].col_type, ColumnType::Nvarchar(Some(100)));
                assert_eq!(ct.columns[6].col_type, ColumnType::Text);
                assert_eq!(ct.columns[7].col_type, ColumnType::Float);
                assert_eq!(ct.columns[8].col_type, ColumnType::Real);
                assert_eq!(ct.columns[9].col_type, ColumnType::Decimal(Some(18), Some(2)));
                assert_eq!(ct.columns[10].col_type, ColumnType::Numeric(Some(10), Some(4)));
                assert_eq!(ct.columns[11].col_type, ColumnType::Bit);
                assert_eq!(ct.columns[12].col_type, ColumnType::Boolean);
                assert_eq!(ct.columns[13].col_type, ColumnType::Date);
                assert_eq!(ct.columns[14].col_type, ColumnType::Timestamp);
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_default_and_identity() {
        let sql = "CREATE TABLE t (
            id INT IDENTITY(1,1) PRIMARY KEY,
            active BIT DEFAULT 1 NOT NULL,
            created DATE DEFAULT '2026-01-01'
        )";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert!(ct.columns[0].identity);
                assert!(ct.columns[0].primary_key);
                assert_eq!(ct.columns[1].default, Some("1".into()));
                assert!(ct.columns[1].not_null);
                assert_eq!(ct.columns[2].default, Some("'2026-01-01'".into()));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_if_not_exists() {
        let sql = "CREATE TABLE IF NOT EXISTS t (id INT)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => assert!(ct.if_not_exists),
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_drop_table() {
        let sql = "DROP TABLE IF EXISTS users";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Drop(dt) => {
                assert_eq!(dt.schema, "dbo");
                assert_eq!(dt.name, "users");
                assert!(dt.if_exists);
            }
            _ => panic!("expected Drop"),
        }
    }

    #[test]
    fn parse_drop_qualified() {
        let sql = "DROP TABLE HR.OldEmployees";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Drop(dt) => {
                assert_eq!(dt.schema, "HR");
                assert_eq!(dt.name, "OldEmployees");
                assert!(!dt.if_exists);
            }
            _ => panic!("expected Drop"),
        }
    }

    #[test]
    fn parse_create_schema() {
        let sql = "CREATE SCHEMA HR";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::CreateSchema(name) => assert_eq!(name, "HR"),
            _ => panic!("expected CreateSchema"),
        }
    }

    #[test]
    fn parse_references() {
        let sql = "CREATE TABLE orders (
            id INT PRIMARY KEY,
            user_id INT REFERENCES users(id)
        )";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.columns[1].references, Some(("users".into(), "id".into())));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn not_ddl_returns_none() {
        assert!(parse_ddl("SELECT 1").unwrap().is_none());
        assert!(parse_ddl("INSERT INTO t VALUES (1)").unwrap().is_none());
    }
}
