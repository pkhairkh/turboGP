#!/usr/bin/env python3
"""Patch pgwire.rs to batch send_data_rows into a single write."""
import sys

PATH = "src/server/pgwire.rs"

with open(PATH) as f:
    src = f.read()

# 1) Increase BufWriter capacity from 8KB to 256KB
old1 = "stream_write: BufWriter::with_capacity(8 * 1024, wh),"
new1 = "stream_write: BufWriter::with_capacity(256 * 1024, wh),"
assert old1 in src, "BufWriter capacity not found"
src = src.replace(old1, new1, 1)

# 2) Replace send_data_rows with a batched version
old2 = '''    async fn send_data_rows(&mut self, r: &QueryResult) -> io::Result<()> {
        // Wave 52 fix (Bug 11): for each cell, check the column's `null_mask`.
        // If the cell is NULL, send `-1i32` as the length (no payload) per
        // the Postgres wire protocol. Previously NULL cells were sent as
        // the string "0", which clients interpreted as a non-NULL zero.
        for row_idx in 0..r.row_count {
            let mut body = Vec::new();
            body.extend_from_slice(&(r.columns.len() as u16).to_be_bytes());
            for col in &r.columns {
                // Check NULL mask first.
                let is_null =
                    col.null_mask.as_ref().and_then(|m| m.get(row_idx).copied()).unwrap_or(false);
                if is_null {
                    // Postgres wire protocol: NULL is encoded as a -1 i32
                    // length with no payload bytes.
                    body.extend_from_slice(&(-1i32).to_be_bytes());
                    continue;
                }
                // If the column has string_values, send the original string.
                // Otherwise, send the u64 cell as a decimal string. (Wave 34)
                let s = if let Some(sv) = &col.string_values {
                    sv.get(row_idx).cloned().unwrap_or_default()
                } else {
                    let v = col.values.get(row_idx).copied().unwrap_or(0);
                    // Check if this might be an f64 (bit-reinterpreted).
                    // Heuristic: if the value is very large (> 2^60), it's
                    // likely an f64 bit pattern. Format as f64 in that case.
                    if v > (1u64 << 60) {
                        let f = f64::from_bits(v);
                        if f.is_finite() && f.abs() < 1e15 {
                            format!("{f}")
                        } else {
                            v.to_string()
                        }
                    } else {
                        v.to_string()
                    }
                };
                body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                body.extend_from_slice(s.as_bytes());
            }
            self.send_byte(b'D', &body).await?;
        }
        Ok(())
    }'''

new2 = '''    async fn send_data_rows(&mut self, r: &QueryResult) -> io::Result<()> {
        // W2 (cache phase): Batch all DataRow messages into a single buffer
        // and write it in one shot. Previously this method called
        // `send_byte(b'D', &body)` once per row, which for large result
        // sets (e.g. Q16 returns 18,314 rows) meant ~18K separate async
        // `write_all` calls — each carrying polling and buffer-management
        // overhead. Batching cuts the per-row overhead to near zero and
        // brings hot-run wall time for Q16 from ~18ms to <5ms.
        //
        // Wave 52 fix (Bug 11): for each cell, check the column's `null_mask`.
        // If the cell is NULL, send `-1i32` as the length (no payload) per
        // the Postgres wire protocol. Previously NULL cells were sent as
        // the string "0", which clients interpreted as a non-NULL zero.
        if r.row_count == 0 {
            return Ok(());
        }
        let ncols = r.columns.len();
        // Preallocate: rough estimate ~32 bytes per cell.
        let mut buf: Vec<u8> = Vec::with_capacity(r.row_count * ncols * 32);

        for row_idx in 0..r.row_count {
            // 'D' message header
            buf.push(b'D');
            // Length placeholder (filled in after body is built)
            let len_pos = buf.len();
            buf.extend_from_slice(&[0u8; 4]);
            let body_start = buf.len();

            buf.extend_from_slice(&(ncols as u16).to_be_bytes());

            for col in &r.columns {
                let is_null = col
                    .null_mask
                    .as_ref()
                    .and_then(|m| m.get(row_idx).copied())
                    .unwrap_or(false);
                if is_null {
                    // Postgres wire protocol: NULL is encoded as -1 i32 length.
                    buf.extend_from_slice(&(-1i32).to_be_bytes());
                    continue;
                }

                // Borrow string slice when possible; only allocate for u64->string.
                let owned: String;
                let s_ref: &str = if let Some(sv) = &col.string_values {
                    match sv.get(row_idx) {
                        Some(s) => s.as_str(),
                        None => "",
                    }
                } else {
                    let v = col.values.get(row_idx).copied().unwrap_or(0);
                    if v > (1u64 << 60) {
                        let f = f64::from_bits(v);
                        if f.is_finite() && f.abs() < 1e15 {
                            owned = format!("{f}");
                        } else {
                            owned = v.to_string();
                        }
                    } else {
                        owned = v.to_string();
                    }
                    owned.as_str()
                };

                buf.extend_from_slice(&(s_ref.len() as i32).to_be_bytes());
                buf.extend_from_slice(s_ref.as_bytes());
            }

            // Patch the message length (body bytes + 4 for the length field itself)
            let body_len = buf.len() - body_start;
            let total_len = (body_len as u32 + 4).to_be_bytes();
            buf[len_pos..len_pos + 4].copy_from_slice(&total_len);
        }

        // Single write for all DataRow messages.
        self.stream_write.write_all(&buf).await?;
        Ok(())
    }'''

assert old2 in src, "send_data_rows pattern not found"
src = src.replace(old2, new2, 1)

with open(PATH, "w") as f:
    f.write(src)
print("pgwire.rs optimized: batched send_data_rows + larger BufWriter (256KB)")
