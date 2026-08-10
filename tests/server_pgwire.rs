//! Wave 2 — Postgres wire protocol server integration test.
//! Boots a real turboGP server on an ephemeral port, connects via raw TCP,
//! and verifies the full protocol: startup, simple query, error handling,
//! multi-statement batch.

use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use turbogp::engine::QueryEngine;
use turbogp::server::{Server, ServerConfig};

fn make_engine() -> QueryEngine {
    use turbogp::datasource::parquet::{LoadedColumn, LoadedTable};
    use turbogp::datasource::Table as DS;
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
                cells: vec![10, 20, 30],
                row_count: 3,
                string_search: None,
                null_bitmap: None,
            },
        ],
        row_count: 3,
    });
    let mut e = QueryEngine::new();
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
        // SSLRequest
        self.s.write_all(&8i32.to_be_bytes()).await?;
        self.s.write_all(&80877103i32.to_be_bytes()).await?;
        self.s.flush().await?;
        let mut b = [0u8; 1];
        self.s.read_exact(&mut b).await?;
        assert_eq!(b[0], b'N');
        // StartupMessage
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
            let (t, body) = self.read_msg().await?;
            match t {
                b'R' => {
                    assert_eq!(body.len(), 4);
                    assert_eq!(&body[..], &[0, 0, 0, 0]);
                }
                b'S' | b'K' => {}
                b'Z' => return Ok(()),
                b'E' => {
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, parse_err(&body)))
                }
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

fn parse_err(body: &[u8]) -> String {
    let mut i = 0;
    let mut msg = String::new();
    while i < body.len() && body[i] != 0 {
        let f = body[i] as char;
        i += 1;
        let end = body[i..].iter().position(|&b| b == 0).unwrap_or(body.len() - i);
        if f == 'M' {
            msg = String::from_utf8_lossy(&body[i..i + end]).into_owned();
        }
        i += end + 1;
    }
    msg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn count_star() {
    let addr = boot(make_engine()).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();
    c.send_query("SELECT count(*) FROM t").await.unwrap();
    let mut got_data = false;
    loop {
        let (t, body) = c.read_msg().await.unwrap();
        match t {
            b'T' => {}
            b'D' => {
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                assert_eq!(n, 1);
                let cl = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
                assert_eq!(std::str::from_utf8(&body[6..6 + cl]).unwrap(), "3");
                got_data = true;
            }
            b'C' => {}
            b'Z' => break,
            _ => panic!("unexpected msg {t:#x}"),
        }
    }
    assert!(got_data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_on_missing_table() {
    let addr = boot(make_engine()).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();
    c.send_query("SELECT count(*) FROM nope").await.unwrap();
    let (t, body) = c.read_msg().await.unwrap();
    assert_eq!(t, b'E');
    let msg = parse_err(&body);
    assert!(
        msg.to_lowercase().contains("not found") || msg.to_lowercase().contains("nope"),
        "got: {msg}"
    );
    let (t, _) = c.read_msg().await.unwrap();
    assert_eq!(t, b'Z');
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_statement_batch() {
    let addr = boot(make_engine()).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();
    c.send_query("SELECT count(*) FROM t; SELECT count(*) FROM t").await.unwrap();
    let mut rows = 0;
    let mut completes = 0;
    loop {
        let (t, _) = c.read_msg().await.unwrap();
        match t {
            b'D' => rows += 1,
            b'C' => completes += 1,
            b'Z' => break,
            _ => {}
        }
    }
    assert!(rows >= 1 && completes >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn select_with_where() {
    let addr = boot(make_engine()).await;
    let mut c = PgClient::connect(addr).await.unwrap();
    c.send_startup("turboGP", "turboGP").await.unwrap();
    c.read_until_ready().await.unwrap();
    c.send_query("SELECT id FROM t WHERE v = 20").await.unwrap();
    let mut got = false;
    loop {
        let (t, body) = c.read_msg().await.unwrap();
        match t {
            b'D' => {
                let cl = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
                assert_eq!(std::str::from_utf8(&body[6..6 + cl]).unwrap(), "2");
                got = true;
            }
            b'T' | b'C' => {}
            b'Z' => break,
            _ => panic!("unexpected {t:#x}"),
        }
    }
    assert!(got);
}
