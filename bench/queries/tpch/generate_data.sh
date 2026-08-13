#!/usr/bin/env bash
# Regenerate TPC-H data at SF=1 and SF=10.
# Usage: bash benchmarks/tpch/generate_data.sh
set -euo pipefail
REPO_ROOT="$(git rev-parse --show-toplevel)"
DBGEN_DIR="$REPO_ROOT/benchmarks/tpch/dbgen-src"
DATA_DIR="$REPO_ROOT/benchmarks/tpch/data"

if [ ! -x "$DBGEN_DIR/dbgen" ]; then
    echo "dbgen not found; cloning and compiling..."
    mkdir -p "$DBGEN_DIR"
    git clone --depth 1 https://github.com/electrum/tpch-dbgen.git "$DBGEN_DIR"
    cd "$DBGEN_DIR"
    sed -i 's/^CC.*$/CC      = gcc/' Makefile
    sed -i 's/@:char/@:string/g' varsub.h 2>/dev/null || true
    make -j4
else
    cd "$DBGEN_DIR"
fi

mkdir -p "$DATA_DIR/sf1" "$DATA_DIR/sf10"

for SF in 1 10; do
    echo "=== Generating SF=$SF ==="
    ./dbgen -f -s "$SF"
    mv customer.tbl lineitem.tbl nation.tbl orders.tbl partsupp.tbl part.tbl region.tbl supplier.tbl "$DATA_DIR/sf$SF/"
    echo "SF=$SF done. Files in $DATA_DIR/sf$SF/"
done

echo "=== Row counts ==="
for SF in 1 10; do
    echo "SF=$SF:"
    for tbl in customer lineitem nation orders partsupp part region supplier; do
        wc -l "$DATA_DIR/sf$SF/$tbl.tbl"
    done
done
