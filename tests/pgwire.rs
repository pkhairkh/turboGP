//! Wave 52 — End-to-end pgwire protocol tests for the three bugs fixed in
//! this wave:
//!
//! 1. NULL values were sent as the string "0" instead of a NULL indicator
//!    (length = -1). Now `send_data_rows` checks `ResultColumn::null_mask`
//!    and emits a -1 i32 length for NULL cells.
//! 2. Describe (D message with kind 'P') executed the query as a side
//!    effect of learning its result shape. Now Describe always returns
//!    NoData without executing.
//! 3. max_rows in the Execute message was discarded. Now Execute honours
//!    max_rows: it sends at most max_rows DataRow messages and signals
//!    PortalSuspended ('s') when more rows remain.

use parking_lot::RwLock;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use turbogp::engine::QueryEngine;
use turbogp::server::{Server, ServerConfig};

fn make_engine_with_nulls() -> QueryEngine {
    use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
    use turbogp::datasource::Table as DS;
    // Build a table where row 1 has a NULL in the `v` column.
    let t = DS::from_loaded(LoadedTable {
        name: "t".into(),
        columns: vec![
            LoadedColumn {
                name: "id".into(),
                cells: vec![1, 2, 3],
                row_count: 3,
                string_search: None,
                null_bitmap: None,
            },
            LoadedColumn {
                name: "v".into(),
                cells: vec![10, 0, 30],
                row_count: 3,
                string_search: None,
                null_bitmap: Some(vec![false, true, false]),
            },
        ],
        row_count: 3,
    });
    let mut e = QueryEngine::in_memory();
    e.register_table(t);
    e
}

fn make_engine_with_rows(n: u64) -> QueryEngine {
    use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
    use turbogp::datasource::Table as DS;
    let t = DS::from_loaded(LoadedTable {
        name: "t".into(),
        columns: vec![LoadedColumn {
            name: "id".into(),
            cells: (1..=n).collect(),
            row_count: n as usize,
            string_search: None,
            null_bitmap: None,
        }],
        row_count: n as usize,
    });
    let mut e = QueryEngine::in_memory();
    e.register_table(t);
    e
}

async fn boot(e: QueryEngine) -> std::net::SocketAddr {
    let e = Arc::new(RwLock::new(e));
    let mut cfg = ServerConfig::default();
    cfg.auth_required = false;
    let s = Server::bind(e, cfg).await.unwrap();
    let a = s.local_addr;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    a
}

struct PgClient {
    s: TcpStream,
}
impl PgClient {
    async fn connect(addr: std::net::SocketAddr) -> std::io::Result<Self> {
        Ok(PgClient { s: TcpStream::connect(addr).await? })
    }
    async fn send_startup(&mut self, user: &str, db: &str) -> std::io::Result<()> {
        self.s.write_all(&8i32.to_be_bytes()).await?;
        self.s.write_all(&80877103i32.to_be_bytes()).await?;
        self.s.flush().await?;
        let mut b = [0u8; 1];
        self.s.read_exact(&mut b).await?;
        assert_eq!(b[0], b'N');
        let mut body = Vec::new();
        body.extend_from_slice(&196608i32.to_be_bytes());
        body.extend_from_slice(b"user\0");
        body.extend_from_slice(user.as_bytes());
        body.push(0);
        body.extend_from_slice(b"database\0");
        body.extend_from_slice(db.as_bytes());
        body.push(0);
        body.push(0);
        self.s.write_all(&((body.len() + 4) as i32).to_be_bytes()).await?;
        self.s.write_all(&body).await?;
        self.s.flush().await
    }
    async fn read_until_ready(&mut self) -> std::io::Result<()> {
        loop {
            let (t, _body) = self.read_msg().await?;
            match t {
                b'R' | b'S' | b'K' => {}
                b'Z' => return Ok(()),
                b'E' => return Ok(()), // tolerate error for test setup
                _ => {}
            }
        }
    }
    async fn send_query(&mut self, sql: &str) -> std::io::Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        self.s.write_all(b"Q").await?;
        self.s.write_all(&((body.len() + 4) as i32).to_be_bytes()).await?;
        self.s.write_all(&body).await?;
        self.s.flush().await
    }
    async fn send_parse(&mut self, stmt_name: &str, sql: &str) -> std::io::Result<()> {
        // Parse: 'P' + len + stmt_name\0 + sql\0 + n_params(0)
        let mut body = Vec::new();
        body.extend_from_slice(stmt_name.as_bytes());
        body.push(0);
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        body.extend_from_slice(&0u16.to_be_bytes());
        self.s.write_all(b"P").await?;
        self.s.write_all(&((body.len() + 4) as i32).to_be_bytes()).await?;
        self.s.write_all(&body).await?;
        self.s.flush().await
    }
    async fn send_bind(&mut self, portal_name: &str, stmt_name: &str) -> std::io::Result<()> {
        // Bind: 'B' + len + portal\0 + stmt\0 + 0 + 0 + 0
        let mut body = Vec::new();
        body.extend_from_slice(portal_name.as_bytes());
        body.push(0);
        body.extend_from_slice(stmt_name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0u16.to_be_bytes()); // 0 param formats
        body.extend_from_slice(&0u16.to_be_bytes()); // 0 params
        body.extend_from_slice(&0u16.to_be_bytes()); // 0 result formats
        self.s.write_all(b"B").await?;
        self.s.write_all(&((body.len() + 4) as i32).to_be_bytes()).await?;
        self.s.write_all(&body).await?;
        self.s.flush().await
    }
    async fn send_describe_portal(&mut self, portal_name: &str) -> std::io::Result<()> {
        // Describe: 'D' + len + 'P' + portal\0
        let mut body = Vec::new();
        body.push(b'P');
        body.extend_from_slice(portal_name.as_bytes());
        body.push(0);
        self.s.write_all(b"D").await?;
        self.s.write_all(&((body.len() + 4) as i32).to_be_bytes()).await?;
        self.s.write_all(&body).await?;
        self.s.flush().await
    }
    async fn send_execute(&mut self, portal_name: &str, max_rows: i32) -> std::io::Result<()> {
        // Execute: 'E' + len + portal\0 + max_rows
        let mut body = Vec::new();
        body.extend_from_slice(portal_name.as_bytes());
        body.push(0);
        body.extend_from_slice(&max_rows.to_be_bytes());
        self.s.write_all(b"E").await?;
        self.s.write_all(&((body.len() + 4) as i32).to_be_bytes()).await?;
        self.s.write_all(&body).await?;
        self.s.flush().await
    }
    async fn send_sync(&mut self) -> std::io::Result<()> {
        self.s.write_all(b"S").await?;
        self.s.write_all(&4i32.to_be_bytes()).await?;
        self.s.flush().await
    }
    async fn read_msg(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut h = [0u8; 5];
        self.s.read_exact(&mut h).await?;
        let t = h[0];
        let len = i32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        self.s.read_exact(&mut body).await?;
        Ok((t, body))
    }
}

// -----------------------------------------------------------------------
// Bug 11: NULL values sent as -1 length, not "0".
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pgwire_null_cell_emits_minus_one_length() {
    let addr = boot(make_engine_with_nulls()).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();
    // SELECT id, v FROM t — row 1 (id=2, v=NULL) must have v as NULL.
    c.send_query("SELECT id, v FROM t").await.unwrap();

    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    loop {
        let (t, body) = c.read_msg().await.unwrap();
        match t {
            b'T' => {} // RowDescription
            b'D' => {
                // DataRow: u16 ncols, then for each col: i32 length + bytes (or -1 for NULL).
                let ncols = u16::from_be_bytes([body[0], body[1]]) as usize;
                let mut row: Vec<Option<String>> = Vec::with_capacity(ncols);
                let mut off = 2;
                for _ in 0..ncols {
                    let len = i32::from_be_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                    ]);
                    off += 4;
                    if len < 0 {
                        row.push(None);
                    } else {
                        let len = len as usize;
                        let s = String::from_utf8_lossy(&body[off..off + len]).into_owned();
                        row.push(Some(s));
                        off += len;
                    }
                }
                rows.push(row);
            }
            b'C' => {} // CommandComplete
            b'Z' => break,
            _ => {}
        }
    }

    assert_eq!(rows.len(), 3, "expected 3 rows");
    // Row 0: id=1, v=10
    assert_eq!(rows[0][0], Some("1".to_string()));
    assert_eq!(rows[0][1], Some("10".to_string()));
    // Row 1: id=2, v=NULL (the crucial assertion)
    assert_eq!(rows[1][0], Some("2".to_string()));
    assert_eq!(rows[1][1], None, "NULL cell must be sent as NULL (length=-1), not \"0\"");
    // Row 2: id=3, v=30
    assert_eq!(rows[2][0], Some("3".to_string()));
    assert_eq!(rows[2][1], Some("30".to_string()));
}

// -----------------------------------------------------------------------
// Bug 12: Describe does not execute the query.
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pgwire_describe_does_not_execute() {
    let addr = boot(make_engine_with_rows(5)).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();

    // Parse + Bind + Describe (portal) + Sync, without Execute.
    // The query counts rows in `t`. If Describe executed it, the engine
    // would have run the SELECT — but we never see a DataRow, only NoData.
    c.send_parse("stmt", "SELECT count(*) FROM t").await.unwrap();
    c.send_bind("portal", "stmt").await.unwrap();
    c.send_describe_portal("portal").await.unwrap();
    c.send_sync().await.unwrap();

    let mut saw_nodata = false;
    let mut saw_datarrow = false;
    loop {
        let (t, _body) = c.read_msg().await.unwrap();
        match t {
            b'1' => {} // ParseComplete
            b'2' => {} // BindComplete
            b'n' => {
                saw_nodata = true;
            } // NoData
            b'T' => {} // RowDescription (would mean query was executed)
            b'D' => {
                saw_datarrow = true;
            } // DataRow (would mean query was executed)
            b'Z' => break,
            _ => {}
        }
    }
    assert!(saw_nodata, "Describe must send NoData");
    assert!(!saw_datarrow, "Describe must NOT execute the query (no DataRow allowed)");
}

// -----------------------------------------------------------------------
// Bug 13: max_rows limits the number of DataRow messages.
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pgwire_max_rows_limits_data_rows() {
    let addr = boot(make_engine_with_rows(10)).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();

    // Parse + Bind + Execute(max_rows=2) + Sync.
    c.send_parse("stmt", "SELECT id FROM t").await.unwrap();
    c.send_bind("portal", "stmt").await.unwrap();
    c.send_execute("portal", 2).await.unwrap();
    c.send_sync().await.unwrap();

    let mut data_rows = 0;
    let mut saw_suspended = false;
    let mut saw_complete = false;
    loop {
        let (t, _body) = c.read_msg().await.unwrap();
        match t {
            b'1' | b'2' => {} // ParseComplete, BindComplete
            b'D' => {
                data_rows += 1;
            }
            b's' => {
                saw_suspended = true;
            } // PortalSuspended
            b'C' => {
                saw_complete = true;
            } // CommandComplete
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(data_rows, 2, "max_rows=2 must limit Execute to 2 DataRow messages");
    assert!(saw_suspended, "PortalSuspended must be sent when more rows remain");
    assert!(!saw_complete, "CommandComplete must NOT be sent while rows remain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pgwire_max_rows_zero_sends_all() {
    let addr = boot(make_engine_with_rows(5)).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();

    c.send_parse("stmt", "SELECT id FROM t").await.unwrap();
    c.send_bind("portal", "stmt").await.unwrap();
    c.send_execute("portal", 0).await.unwrap(); // max_rows = 0 = unlimited
    c.send_sync().await.unwrap();

    let mut data_rows = 0;
    let mut saw_complete = false;
    loop {
        let (t, _body) = c.read_msg().await.unwrap();
        match t {
            b'1' | b'2' => {}
            b'D' => {
                data_rows += 1;
            }
            b'C' => {
                saw_complete = true;
            }
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(data_rows, 5, "max_rows=0 must send all 5 rows");
    assert!(saw_complete, "CommandComplete must be sent when max_rows=0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pgwire_cursor_drains_remaining_rows() {
    // FETCH 2 FROM c; FETCH 2 FROM c; FETCH 100 FROM c;
    // Should return 2 + 2 + 1 rows from a 5-row table.
    let addr = boot(make_engine_with_rows(5)).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();

    c.send_parse("stmt", "SELECT id FROM t").await.unwrap();
    c.send_bind("portal", "stmt").await.unwrap();

    // First Execute: max_rows=2 → 2 rows + PortalSuspended.
    c.send_execute("portal", 2).await.unwrap();
    c.send_sync().await.unwrap();
    let mut batch1 = 0;
    loop {
        let (t, _b) = c.read_msg().await.unwrap();
        match t {
            b'1' | b'2' => {}
            b'D' => {
                batch1 += 1;
            }
            b's' => break,
            b'C' => panic!("should be suspended, not complete"),
            b'Z' => break,
            _ => {}
        }
    }
    // Drain until ReadyForQuery.
    loop {
        let (t, _b) = c.read_msg().await.unwrap();
        if t == b'Z' {
            break;
        }
    }
    assert_eq!(batch1, 2, "first Execute(max_rows=2) must return 2 rows");

    // Second Execute: max_rows=2 → 2 more rows + PortalSuspended.
    c.send_execute("portal", 2).await.unwrap();
    c.send_sync().await.unwrap();
    let mut batch2 = 0;
    let mut suspended2 = false;
    loop {
        let (t, _b) = c.read_msg().await.unwrap();
        match t {
            b'D' => {
                batch2 += 1;
            }
            b's' => {
                suspended2 = true;
            }
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(batch2, 2, "second Execute(max_rows=2) must return 2 rows");
    assert!(suspended2, "second Execute must still be suspended (1 row left)");

    // Third Execute: max_rows=100 → 1 remaining row + CommandComplete.
    c.send_execute("portal", 100).await.unwrap();
    c.send_sync().await.unwrap();
    let mut batch3 = 0;
    let mut complete3 = false;
    loop {
        let (t, _b) = c.read_msg().await.unwrap();
        match t {
            b'D' => {
                batch3 += 1;
            }
            b'C' => {
                complete3 = true;
            }
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(batch3, 1, "third Execute must drain the remaining 1 row");
    assert!(complete3, "CommandComplete must be sent when the cursor is exhausted");
}
