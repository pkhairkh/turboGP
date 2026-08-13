-- TPC-H schema for turboGP.
-- turboGP speaks pgwire (PostgreSQL wire protocol). It supports a subset of
-- PostgreSQL types. We use the same column types as the PostgreSQL schema.
-- NOTE: turboGP may not support all PostgreSQL features (e.g. CHAR(N) fixed-length).
-- If turboGP rejects CHAR(N), the load script will retry with VARCHAR.
-- See benchmarks/tpch/schema/turbogp_fallback.sql for the relaxed variant.

DROP TABLE IF EXISTS lineitem;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS customer;
DROP TABLE IF EXISTS part;
DROP TABLE IF EXISTS partsupp;
DROP TABLE IF EXISTS supplier;
DROP TABLE IF EXISTS nation;
DROP TABLE IF EXISTS region;

CREATE TABLE region  (
    r_regionkey  BIGINT,
    r_name       VARCHAR(25),
    r_comment    VARCHAR(152)
);

CREATE TABLE nation  (
    n_nationkey  BIGINT,
    n_name       VARCHAR(25),
    n_regionkey  BIGINT,
    n_comment    VARCHAR(152)
);

CREATE TABLE part  (
    p_partkey     BIGINT,
    p_name        VARCHAR(55),
    p_mfgr        VARCHAR(25),
    p_brand       VARCHAR(10),
    p_type        VARCHAR(25),
    p_size        INTEGER,
    p_container   VARCHAR(10),
    p_retailprice DECIMAL(15,2),
    p_comment     VARCHAR(23)
);

CREATE TABLE supplier  (
    s_suppkey     BIGINT,
    s_name        VARCHAR(25),
    s_address     VARCHAR(40),
    s_nationkey   BIGINT,
    s_phone       VARCHAR(15),
    s_acctbal     DECIMAL(15,2),
    s_comment     VARCHAR(101)
);

CREATE TABLE partsupp  (
    ps_partkey     BIGINT,
    ps_suppkey     BIGINT,
    ps_availqty    INTEGER,
    ps_supplycost  DECIMAL(15,2),
    ps_comment     VARCHAR(199)
);

CREATE TABLE customer  (
    c_custkey      BIGINT,
    c_name         VARCHAR(25),
    c_address      VARCHAR(40),
    c_nationkey    BIGINT,
    c_phone        VARCHAR(15),
    c_acctbal      DECIMAL(15,2),
    c_mktsegment   VARCHAR(10),
    c_comment      VARCHAR(117)
);

CREATE TABLE orders  (
    o_orderkey       BIGINT,
    o_custkey        BIGINT,
    o_orderstatus    VARCHAR(1),
    o_totalprice     DECIMAL(15,2),
    o_orderdate      DATE,
    o_orderpriority  VARCHAR(15),
    o_clerk          VARCHAR(15),
    o_shippriority   INTEGER,
    o_comment        VARCHAR(79)
);

CREATE TABLE lineitem  (
    l_orderkey        BIGINT,
    l_partkey         BIGINT,
    l_suppkey         BIGINT,
    l_linenumber      INTEGER,
    l_quantity        DECIMAL(15,2),
    l_extendedprice   DECIMAL(15,2),
    l_discount        DECIMAL(15,2),
    l_tax             DECIMAL(15,2),
    l_returnflag      VARCHAR(1),
    l_linestatus      VARCHAR(1),
    l_shipdate        DATE,
    l_commitdate      DATE,
    l_receiptdate     DATE,
    l_shipinstruct    VARCHAR(25),
    l_shipmode        VARCHAR(10),
    l_comment         VARCHAR(44)
);
