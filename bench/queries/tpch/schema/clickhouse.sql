-- TPC-H schema for ClickHouse.
-- ClickHouse types differ from PostgreSQL: INTEGER/BIGINT → Int32/Int64,
-- DECIMAL(15,2) → Decimal(15,2), CHAR(N) → String (ClickHouse has no fixed-char),
-- DATE → Date. No PRIMARY KEY (ClickHouse uses ORDER BY for the storage index).
-- Replaces Exasol dialect: Exasol would have used DECIMAL and VARCHAR similar to PG;
-- since Exasol fell back to PostgreSQL, the ClickHouse/Postgres schemas are the two
-- distinct OLAP-vs-OLTP reference points.

DROP TABLE IF EXISTS lineitem;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS customer;
DROP TABLE IF EXISTS part;
DROP TABLE IF EXISTS partsupp;
DROP TABLE IF EXISTS supplier;
DROP TABLE IF EXISTS nation;
DROP TABLE IF EXISTS region;

CREATE TABLE region  (
    r_regionkey  Int64,
    r_name       String,
    r_comment    String
) ENGINE = MergeTree ORDER BY (r_regionkey);

CREATE TABLE nation  (
    n_nationkey  Int64,
    n_name       String,
    n_regionkey  Int64,
    n_comment    String
) ENGINE = MergeTree ORDER BY (n_nationkey);

CREATE TABLE part  (
    p_partkey     Int64,
    p_name        String,
    p_mfgr        String,
    p_brand       String,
    p_type        String,
    p_size        Int32,
    p_container   String,
    p_retailprice Decimal(15,2),
    p_comment     String
) ENGINE = MergeTree ORDER BY (p_partkey);

CREATE TABLE supplier  (
    s_suppkey     Int64,
    s_name        String,
    s_address     String,
    s_nationkey   Int64,
    s_phone       String,
    s_acctbal     Decimal(15,2),
    s_comment     String
) ENGINE = MergeTree ORDER BY (s_suppkey);

CREATE TABLE partsupp  (
    ps_partkey     Int64,
    ps_suppkey     Int64,
    ps_availqty    Int32,
    ps_supplycost  Decimal(15,2),
    ps_comment     String
) ENGINE = MergeTree ORDER BY (ps_partkey, ps_suppkey);

CREATE TABLE customer  (
    c_custkey      Int64,
    c_name         String,
    c_address      String,
    c_nationkey    Int64,
    c_phone        String,
    c_acctbal      Decimal(15,2),
    c_mktsegment   String,
    c_comment      String
) ENGINE = MergeTree ORDER BY (c_custkey);

CREATE TABLE orders  (
    o_orderkey       Int64,
    o_custkey        Int64,
    o_orderstatus    String,
    o_totalprice     Decimal(15,2),
    o_orderdate      Date,
    o_orderpriority  String,
    o_clerk          String,
    o_shippriority   Int32,
    o_comment        String
) ENGINE = MergeTree ORDER BY (o_orderkey);

CREATE TABLE lineitem  (
    l_orderkey        Int64,
    l_partkey         Int64,
    l_suppkey         Int64,
    l_linenumber      Int32,
    l_quantity        Decimal(15,2),
    l_extendedprice   Decimal(15,2),
    l_discount        Decimal(15,2),
    l_tax             Decimal(15,2),
    l_returnflag      String,
    l_linestatus      String,
    l_shipdate        Date,
    l_commitdate      Date,
    l_receiptdate     Date,
    l_shipinstruct    String,
    l_shipmode        String,
    l_comment         String
) ENGINE = MergeTree ORDER BY (l_orderkey, l_linenumber);
