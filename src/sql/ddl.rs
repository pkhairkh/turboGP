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
    /// True if UNIQUE was specified at the column level (Wave 6).
    pub unique: bool,
    /// Optional CHECK constraint expression at the column level (Wave 6).
    pub check: Option<crate::sql::ast::Expr>,
    /// Optional ON DELETE action for the foreign key (Wave 6).
    pub on_delete: Option<ForeignKeyAction>,
    /// Optional ON UPDATE action for the foreign key (Wave 6).
    pub on_update: Option<ForeignKeyAction>,
}

/// Foreign key referential action (Wave 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignKeyAction {
    /// `ON DELETE CASCADE` / `ON UPDATE CASCADE` — propagate the change.
    Cascade,
    /// `ON DELETE SET NULL` / `ON UPDATE SET NULL` — set referencing column to NULL.
    SetNull,
    /// `ON DELETE SET DEFAULT` / `ON UPDATE SET DEFAULT` — set to DEFAULT value.
    SetDefault,
    /// `ON DELETE RESTRICT` / `ON UPDATE RESTRICT` — forbid the change.
    Restrict,
    /// `ON DELETE NO ACTION` / `ON UPDATE NO ACTION` — default; same as RESTRICT
    /// but deferrable.
    NoAction,
}

impl ForeignKeyAction {
    /// Parse a foreign key action from a SQL keyword string.
    ///
    /// Returns `None` for unrecognized strings.
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "CASCADE" => Some(ForeignKeyAction::Cascade),
            "SET" => Some(ForeignKeyAction::SetNull), // caller must check for NULL
            "RESTRICT" => Some(ForeignKeyAction::Restrict),
            "NO" => Some(ForeignKeyAction::NoAction), // caller must check for ACTION
            _ => None,
        }
    }
}

/// A table-level foreign key definition (Wave 6).
#[derive(Debug, Clone)]
pub struct TableForeignKey {
    /// The referencing columns (in the current table).
    pub columns: Vec<String>,
    /// The referenced table name.
    pub ref_table: String,
    /// The referenced columns (in the referenced table).
    pub ref_columns: Vec<String>,
    /// Optional ON DELETE action.
    pub on_delete: Option<ForeignKeyAction>,
    /// Optional ON UPDATE action.
    pub on_update: Option<ForeignKeyAction>,
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
    /// Table-level CHECK constraints (Wave 6). Each is an expression that
    /// must evaluate to TRUE for every row.
    pub checks: Vec<crate::sql::ast::Expr>,
    /// Table-level UNIQUE constraints (Wave 6). Each is a list of columns
    /// whose combination must be unique across rows.
    pub unique_constraints: Vec<Vec<String>>,
    /// Table-level PRIMARY KEY (Wave 6). Composite key if multiple columns.
    pub primary_key: Option<Vec<String>>,
    /// Table-level FOREIGN KEY constraints (Wave 6).
    pub foreign_keys: Vec<TableForeignKey>,
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
///
/// Wave 6: now supports multi-column indexes with per-column sort order.
/// `columns` replaces the single `column` field; each entry is
/// `(column_name, ascending)`.
#[derive(Debug, Clone)]
pub struct CreateIndex {
    /// Index name.
    pub index_name: String,
    /// Table to index.
    pub table: String,
    /// Column to index (Wave 66 — kept for backward compatibility with
    /// single-column indexes). Multi-column indexes should use `columns`.
    pub column: String,
    /// True if `IF NOT EXISTS` was specified.
    pub if_not_exists: bool,
    /// True if `CREATE UNIQUE INDEX` was specified (Wave 6).
    pub unique: bool,
    /// Multi-column index spec (Wave 6). Each entry is `(column_name, ascending)`.
    /// For single-column indexes, this contains one entry matching `column`.
    pub columns: Vec<(String, bool)>,
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
    // Wave 6: CREATE UNIQUE INDEX — the UNIQUE keyword precedes INDEX.
    // Strip both UNIQUE and INDEX, then pass the rest to
    // parse_create_index with the unique flag forced on.
    if let Token::Keyword(k) = &tokens[0] {
        if k == "UNIQUE" {
            if tokens.len() < 2 || !matches!(&tokens[1], Token::Keyword(k) if k == "INDEX") {
                return Err("expected INDEX after CREATE UNIQUE".into());
            }
            let mut ci = parse_create_index(&tokens[2..])?;
            ci.unique = true;
            return Ok(DdlStatement::CreateIndex(ci));
        }
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
    // Optional UNIQUE keyword (Wave 6): CREATE UNIQUE INDEX ...
    let mut unique = false;
    if pos < tokens.len() {
        if let Token::Keyword(k) = &tokens[pos] {
            if k == "UNIQUE" {
                unique = true;
                pos += 1;
            }
        }
    }
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
    // Expect ( column [, column ...] ) with optional ASC/DESC per column
    if pos >= tokens.len() {
        return Err("expected ( after table name".into());
    }
    match &tokens[pos] {
        Token::LParen => pos += 1,
        other => return Err(format!("expected (, got {other:?}")),
    }
    let mut columns: Vec<(String, bool)> = Vec::new();
    loop {
        if pos >= tokens.len() {
            return Err("expected column name".into());
        }
        let col_name = match &tokens[pos] {
            Token::Ident(s) => s.clone(),
            other => return Err(format!("expected column name, got {other:?}")),
        };
        pos += 1;
        // Optional ASC / DESC (Wave 6). Default is ASC = true.
        let ascending = if pos < tokens.len() {
            match &tokens[pos] {
                Token::Keyword(k) if k == "ASC" => {
                    pos += 1;
                    true
                }
                Token::Keyword(k) if k == "DESC" => {
                    pos += 1;
                    false
                }
                // Some lexers may treat ASC/DESC as Ident rather than Keyword.
                Token::Ident(s) if s.eq_ignore_ascii_case("ASC") => {
                    pos += 1;
                    true
                }
                Token::Ident(s) if s.eq_ignore_ascii_case("DESC") => {
                    pos += 1;
                    false
                }
                _ => true,
            }
        } else {
            true
        };
        columns.push((col_name, ascending));
        // Expect , or )
        if pos >= tokens.len() {
            return Err("expected , or ) in index column list".into());
        }
        match &tokens[pos] {
            Token::Comma => pos += 1,
            Token::RParen => {
                pos += 1;
                break;
            }
            other => return Err(format!("expected , or ), got {other:?}")),
        }
    }
    // Backward-compat: single-column indexes populate `column` with the
    // first (and only) column name.
    let column = columns
        .first()
        .map(|(c, _)| c.clone())
        .unwrap_or_default();
    // Trailing tokens (e.g. USING btree, method_opt) are ignored.
    Ok(CreateIndex {
        index_name,
        table,
        column,
        if_not_exists,
        unique,
        columns,
    })
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

    // Parse column definitions and table-level constraints
    let mut columns = Vec::new();
    let mut checks: Vec<crate::sql::ast::Expr> = Vec::new();
    let mut unique_constraints: Vec<Vec<String>> = Vec::new();
    let mut primary_key: Option<Vec<String>> = None;
    let mut foreign_keys: Vec<TableForeignKey> = Vec::new();
    loop {
        if pos >= tokens.len() {
            return Err("unterminated column list".into());
        }
        // Check for closing )
        if let Token::RParen = &tokens[pos] {
            pos += 1;
            break;
        }

        // Detect table-level constraints: CHECK (...), UNIQUE (...),
        // PRIMARY KEY (...), FOREIGN KEY (...) REFERENCES ...
        let is_table_constraint = match &tokens[pos] {
            Token::Keyword(k) => matches!(
                k.as_str(),
                "CHECK" | "UNIQUE" | "PRIMARY" | "FOREIGN" | "CONSTRAINT"
            ),
            Token::Ident(s) => matches!(
                s.to_uppercase().as_str(),
                "CHECK" | "UNIQUE" | "PRIMARY" | "FOREIGN" | "CONSTRAINT"
            ),
            _ => false,
        };

        if is_table_constraint {
            parse_table_constraint(
                tokens,
                &mut pos,
                &mut checks,
                &mut unique_constraints,
                &mut primary_key,
                &mut foreign_keys,
            )?;
        } else {
            // Parse one column def
            let col = parse_column_def(tokens, &mut pos)?;
            columns.push(col);
        }

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
    Ok(CreateTable {
        schema,
        name,
        columns,
        if_not_exists,
        checks,
        unique_constraints,
        primary_key,
        foreign_keys,
    })
}

/// Parse a table-level constraint (CHECK / UNIQUE / PRIMARY KEY / FOREIGN KEY).
/// Skips a leading `CONSTRAINT name` prefix if present.
fn parse_table_constraint(
    tokens: &[Token],
    pos: &mut usize,
    checks: &mut Vec<crate::sql::ast::Expr>,
    unique_constraints: &mut Vec<Vec<String>>,
    primary_key: &mut Option<Vec<String>>,
    foreign_keys: &mut Vec<TableForeignKey>,
) -> Result<(), String> {
    // Optional CONSTRAINT <name> prefix.
    let kw = peek_keyword_or_ident(tokens, *pos);
    if kw.as_deref() == Some("CONSTRAINT") {
        *pos += 1;
        // Skip the constraint name.
        if *pos < tokens.len() {
            *pos += 1;
        }
    }
    let kw = peek_keyword_or_ident(tokens, *pos);
    match kw.as_deref() {
        Some("CHECK") => {
            *pos += 1;
            // Expect ( expr )
            expect_lparen(tokens, pos)?;
            let expr = parse_expr_from_tokens_ddl(tokens, pos)?;
            expect_rparen(tokens, pos)?;
            checks.push(expr);
        }
        Some("UNIQUE") => {
            *pos += 1;
            expect_lparen(tokens, pos)?;
            let cols = parse_column_list_paren(tokens, pos)?;
            expect_rparen(tokens, pos)?;
            unique_constraints.push(cols);
        }
        Some("PRIMARY") => {
            *pos += 1;
            // Expect KEY
            let next = peek_keyword_or_ident(tokens, *pos);
            if next.as_deref() != Some("KEY") {
                return Err("expected KEY after PRIMARY".into());
            }
            *pos += 1;
            expect_lparen(tokens, pos)?;
            let cols = parse_column_list_paren(tokens, pos)?;
            expect_rparen(tokens, pos)?;
            *primary_key = Some(cols);
        }
        Some("FOREIGN") => {
            *pos += 1;
            // Expect KEY
            let next = peek_keyword_or_ident(tokens, *pos);
            if next.as_deref() != Some("KEY") {
                return Err("expected KEY after FOREIGN".into());
            }
            *pos += 1;
            expect_lparen(tokens, pos)?;
            let cols = parse_column_list_paren(tokens, pos)?;
            expect_rparen(tokens, pos)?;
            // Expect REFERENCES table (col, ...)
            let ref_kw = peek_keyword_or_ident(tokens, *pos);
            if ref_kw.as_deref() != Some("REFERENCES") {
                return Err("expected REFERENCES after FOREIGN KEY columns".into());
            }
            *pos += 1;
            let (_ref_schema, ref_table) = parse_qualified_name(tokens, pos)?;
            expect_lparen(tokens, pos)?;
            let ref_cols = parse_column_list_paren(tokens, pos)?;
            expect_rparen(tokens, pos)?;
            // Optional ON DELETE / ON UPDATE actions
            let mut on_delete = None;
            let mut on_update = None;
            loop {
                let kw = peek_keyword_or_ident(tokens, *pos);
                match kw.as_deref() {
                    Some("ON") => {
                        *pos += 1;
                        let action_kw = peek_keyword_or_ident(tokens, *pos);
                        match action_kw.as_deref() {
                            Some("DELETE") => {
                                *pos += 1;
                                on_delete = Some(parse_fk_action(tokens, pos)?);
                            }
                            Some("UPDATE") => {
                                *pos += 1;
                                on_update = Some(parse_fk_action(tokens, pos)?);
                            }
                            _ => return Err("expected DELETE or UPDATE after ON".into()),
                        }
                    }
                    _ => break,
                }
            }
            foreign_keys.push(TableForeignKey {
                columns: cols,
                ref_table,
                ref_columns: ref_cols,
                on_delete,
                on_update,
            });
        }
        other => return Err(format!("unknown table constraint: {other:?}")),
    }
    Ok(())
}

/// Parse a foreign key action: CASCADE, SET NULL, SET DEFAULT, RESTRICT,
/// NO ACTION.
fn parse_fk_action(tokens: &[Token], pos: &mut usize) -> Result<ForeignKeyAction, String> {
    let kw = peek_keyword_or_ident(tokens, *pos);
    *pos += 1;
    match kw.as_deref() {
        Some("CASCADE") => Ok(ForeignKeyAction::Cascade),
        Some("SET") => {
            let next = peek_keyword_or_ident(tokens, *pos);
            *pos += 1;
            match next.as_deref() {
                Some("NULL") => Ok(ForeignKeyAction::SetNull),
                Some("DEFAULT") => Ok(ForeignKeyAction::SetDefault),
                _ => Err("expected NULL or DEFAULT after SET".into()),
            }
        }
        Some("RESTRICT") => Ok(ForeignKeyAction::Restrict),
        Some("NO") => {
            let next = peek_keyword_or_ident(tokens, *pos);
            *pos += 1;
            if next.as_deref() == Some("ACTION") {
                Ok(ForeignKeyAction::NoAction)
            } else {
                Err("expected ACTION after NO".into())
            }
        }
        other => Err(format!("expected CASCADE / SET NULL / SET DEFAULT / RESTRICT / NO ACTION, got {other:?}")),
    }
}

/// Peek at the current token, returning its keyword/ident text if it is
/// a Keyword or Ident. Used for case-insensitive constraint keyword matching.
fn peek_keyword_or_ident(tokens: &[Token], pos: usize) -> Option<String> {
    if pos >= tokens.len() {
        return None;
    }
    match &tokens[pos] {
        Token::Keyword(k) => Some(k.clone()),
        Token::Ident(s) => Some(s.to_uppercase()),
        _ => None,
    }
}

fn expect_lparen(tokens: &[Token], pos: &mut usize) -> Result<(), String> {
    if *pos >= tokens.len() || !matches!(&tokens[*pos], Token::LParen) {
        return Err(format!("expected (, got {:?}", tokens.get(*pos)));
    }
    *pos += 1;
    Ok(())
}

fn expect_rparen(tokens: &[Token], pos: &mut usize) -> Result<(), String> {
    if *pos >= tokens.len() || !matches!(&tokens[*pos], Token::RParen) {
        return Err(format!("expected ), got {:?}", tokens.get(*pos)));
    }
    *pos += 1;
    Ok(())
}

/// Parse a comma-separated list of column names (without surrounding parens).
fn parse_column_list_paren(tokens: &[Token], pos: &mut usize) -> Result<Vec<String>, String> {
    let mut cols = Vec::new();
    loop {
        if *pos >= tokens.len() {
            return Err("unterminated column list".into());
        }
        match &tokens[*pos] {
            Token::Ident(s) => cols.push(s.clone()),
            Token::RParen => break,
            other => return Err(format!("expected column or ), got {other:?}")),
        }
        *pos += 1;
        if *pos >= tokens.len() {
            return Err("unterminated column list".into());
        }
        match &tokens[*pos] {
            Token::Comma => *pos += 1,
            Token::RParen => break,
            _ => {}
        }
    }
    Ok(cols)
}

/// Parse an expression from the DDL token stream by delegating to the
/// main parser's `parse_expression` function.
fn parse_expr_from_tokens_ddl(
    tokens: &[Token],
    pos: &mut usize,
) -> Result<crate::sql::ast::Expr, String> {
    // Find the matching ) for the CHECK constraint's expression.
    let start = *pos;
    let mut depth: i32 = 0;
    let mut end = start;
    while end < tokens.len() {
        match &tokens[end] {
            Token::LParen => depth += 1,
            Token::RParen => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            Token::Comma | Token::Semicolon | Token::EOF if depth == 0 => break,
            _ => {}
        }
        end += 1;
    }
    let mut sub_tokens: Vec<Token> = tokens[start..end].to_vec();
    sub_tokens.push(Token::EOF);
    *pos = end;
    crate::sql::parser::parse_expression(sub_tokens)
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
    let mut unique = false;
    let mut check: Option<crate::sql::ast::Expr> = None;
    let mut on_delete: Option<ForeignKeyAction> = None;
    let mut on_update: Option<ForeignKeyAction> = None;

    while *pos < tokens.len() {
        let kw = peek_keyword_or_ident(tokens, *pos);
        match kw.as_deref() {
            Some("NOT") => {
                *pos += 1;
                if *pos >= tokens.len() {
                    return Err("expected NULL after NOT".into());
                }
                let next = peek_keyword_or_ident(tokens, *pos);
                if next.as_deref() != Some("NULL") {
                    return Err(format!("expected NULL after NOT, got {:?}", tokens[*pos]));
                }
                not_null = true;
                *pos += 1;
            }
            Some("NULL") => {
                *pos += 1; // explicit NULL allowed
            }
            Some("PRIMARY") => {
                *pos += 1;
                let next = peek_keyword_or_ident(tokens, *pos);
                if next.as_deref() != Some("KEY") {
                    return Err("expected KEY after PRIMARY".into());
                }
                primary_key = true;
                not_null = true;
                *pos += 1;
            }
            Some("UNIQUE") => {
                unique = true;
                *pos += 1;
            }
            Some("DEFAULT") => {
                *pos += 1;
                if *pos >= tokens.len() {
                    return Err("expected value after DEFAULT".into());
                }
                default = Some(token_to_literal(&tokens[*pos])?);
                *pos += 1;
            }
            Some("IDENTITY") => {
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
            Some("CHECK") => {
                *pos += 1;
                expect_lparen(tokens, pos)?;
                let expr = parse_expr_from_tokens_ddl(tokens, pos)?;
                expect_rparen(tokens, pos)?;
                check = Some(expr);
            }
            Some("REFERENCES") => {
                *pos += 1;
                let (_ref_schema, ref_table) = parse_qualified_name(tokens, pos)?;
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
                // Optional ON DELETE / ON UPDATE actions follow REFERENCES.
                // They are parsed in the next iteration of the loop.
            }
            Some("ON") => {
                *pos += 1;
                let action_kw = peek_keyword_or_ident(tokens, *pos);
                match action_kw.as_deref() {
                    Some("DELETE") => {
                        *pos += 1;
                        on_delete = Some(parse_fk_action(tokens, pos)?);
                    }
                    Some("UPDATE") => {
                        *pos += 1;
                        on_update = Some(parse_fk_action(tokens, pos)?);
                    }
                    _ => return Err("expected DELETE or UPDATE after ON".into()),
                }
            }
            // End of column def
            _ if matches!(&tokens[*pos], Token::Comma | Token::RParen) => break,
            _ => {
                // Skip unknown constraint tokens (e.g. COLLATE, etc.)
                *pos += 1;
            }
        }
    }

    Ok(ColumnDef {
        name,
        col_type,
        not_null,
        primary_key,
        default,
        identity,
        references,
        unique,
        check,
        on_delete,
        on_update,
    })
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

    // ENUM is special: its parenthesised arg is a list of string literals,
    // not numeric length/precision. Parse it directly here.
    if type_name == "ENUM" {
        let mut values: Vec<String> = Vec::new();
        if *pos < tokens.len() {
            if let Token::LParen = &tokens[*pos] {
                *pos += 1;
                loop {
                    if *pos >= tokens.len() {
                        return Err("unterminated ENUM value list".into());
                    }
                    match &tokens[*pos] {
                        Token::String(s) => {
                            values.push(s.clone());
                            *pos += 1;
                        }
                        Token::RParen => {
                            *pos += 1;
                            break;
                        }
                        Token::Comma => {
                            *pos += 1;
                        }
                        other => return Err(format!("expected string literal in ENUM, got {other:?}")),
                    }
                }
            }
        }
        return Ok(ColumnType::Enum(values));
    }

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

    // ===== Wave 6: DDL Expansion Tests =====

    #[test]
    fn parse_check_column_level() {
        let sql = "CREATE TABLE t (x INT CHECK (x > 0))";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.columns.len(), 1);
                assert!(ct.columns[0].check.is_some());
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_check_table_level() {
        let sql = "CREATE TABLE t (x INT, y INT, CHECK (x > y))";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.columns.len(), 2);
                assert_eq!(ct.checks.len(), 1);
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_unique_column_level() {
        let sql = "CREATE TABLE t (email VARCHAR UNIQUE)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => assert!(ct.columns[0].unique),
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_unique_table_level() {
        let sql = "CREATE TABLE t (a INT, b INT, UNIQUE (a, b))";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.unique_constraints.len(), 1);
                assert_eq!(ct.unique_constraints[0], vec!["a".to_string(), "b".to_string()]);
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_multi_column_index() {
        let sql = "CREATE INDEX idx ON t (a, b, c)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::CreateIndex(ci) => {
                assert_eq!(ci.columns.len(), 3);
                assert_eq!(ci.columns[0].0, "a");
                assert_eq!(ci.columns[1].0, "b");
                assert_eq!(ci.columns[2].0, "c");
            }
            _ => panic!("expected CreateIndex"),
        }
    }

    #[test]
    fn parse_index_with_sort_order() {
        let sql = "CREATE INDEX idx ON t (a DESC, b ASC)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::CreateIndex(ci) => {
                assert_eq!(ci.columns.len(), 2);
                assert!(!ci.columns[0].1, "a should be DESC");
                assert!(ci.columns[1].1, "b should be ASC");
            }
            _ => panic!("expected CreateIndex"),
        }
    }

    #[test]
    fn parse_unique_index() {
        let sql = "CREATE UNIQUE INDEX idx ON t (a)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::CreateIndex(ci) => assert!(ci.unique),
            _ => panic!("expected CreateIndex"),
        }
    }

    #[test]
    fn parse_on_delete_cascade() {
        let sql = "CREATE TABLE t (a INT REFERENCES t2(id) ON DELETE CASCADE)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.columns[0].on_delete, Some(ForeignKeyAction::Cascade));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_on_update_set_null() {
        let sql = "CREATE TABLE t (a INT REFERENCES t2(id) ON UPDATE SET NULL)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.columns[0].on_update, Some(ForeignKeyAction::SetNull));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_on_delete_restrict_on_update_cascade() {
        let sql = "CREATE TABLE t (a INT REFERENCES t2(id) ON DELETE RESTRICT ON UPDATE CASCADE)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.columns[0].on_delete, Some(ForeignKeyAction::Restrict));
                assert_eq!(ct.columns[0].on_update, Some(ForeignKeyAction::Cascade));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_enum_values() {
        let sql = "CREATE TABLE t (status ENUM('active', 'inactive', 'pending'))";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => match &ct.columns[0].col_type {
                ColumnType::Enum(values) => {
                    assert_eq!(values.len(), 3);
                    assert_eq!(values[0], "active");
                    assert_eq!(values[1], "inactive");
                    assert_eq!(values[2], "pending");
                }
                other => panic!("expected Enum, got {other:?}"),
            },
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_table_level_primary_key() {
        let sql = "CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b))";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.primary_key, Some(vec!["a".to_string(), "b".to_string()]));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_table_level_foreign_key() {
        let sql = "CREATE TABLE orders (id INT, customer_id INT, FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert_eq!(ct.foreign_keys.len(), 1);
                let fk = &ct.foreign_keys[0];
                assert_eq!(fk.columns, vec!["customer_id".to_string()]);
                assert_eq!(fk.ref_table, "customers");
                assert_eq!(fk.ref_columns, vec!["id".to_string()]);
                assert_eq!(fk.on_delete, Some(ForeignKeyAction::Cascade));
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_combined_constraints() {
        let sql = "CREATE TABLE t (x INT CHECK (x > 0), email VARCHAR UNIQUE)";
        let ddl = parse_ddl(sql).unwrap().unwrap();
        match ddl {
            DdlStatement::Create(ct) => {
                assert!(ct.columns[0].check.is_some());
                assert!(ct.columns[1].unique);
            }
            _ => panic!("expected Create"),
        }
    }
}
