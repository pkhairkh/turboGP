#!/usr/bin/env python3
"""Add explicit flush after sending query results to avoid BufWriter lag."""

PATH = "src/server/pgwire.rs"

with open(PATH) as f:
    src = f.read()

# In handle_simple_query, replace the trailing send_ready_for_query
# with send_ready_for_query + flush, so the 'Z' message doesn't sit in
# the 256KB BufWriter.
old = """        }
        self.send_ready_for_query().await
    }

    // --- Extended query ---"""

new = """        }
        self.send_ready_for_query().await?;
        // W2 (cache phase): explicit flush so the ReadyForQuery ('Z') byte
        // actually reaches the client. With the larger 256KB BufWriter,
        // small trailing messages can sit in the buffer and cause psql to
        // hang waiting for query completion.
        self.flush().await
    }

    // --- Extended query ---"""

assert old in src, "simple query tail not found"
src = src.replace(old, new, 1)

# Also patch the extended-query path (line 777 area): after send_data_rows
# in the extended path, flush before command_complete so large result sets
# stream to the client promptly.
# Look for the block where max_rows = 0:
old2 = """                } else {
                    // max_rows = 0 or result fits in one batch.
                    self.send_data_rows(&r).await?;
                    self.send_command_complete(&tag, r.row_count).await?;
                }"""

new2 = """                } else {
                    // max_rows = 0 or result fits in one batch.
                    self.send_data_rows(&r).await?;
                    self.flush().await?;
                    self.send_command_complete(&tag, r.row_count).await?;
                }"""

assert old2 in src, "extended query batch path not found"
src = src.replace(old2, new2, 1)

with open(PATH, "w") as f:
    f.write(src)
print("pgwire.rs: added explicit flushes after data rows")
