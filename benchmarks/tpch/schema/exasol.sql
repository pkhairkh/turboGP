-- TPC-H schema for Exasol (preserved for reference; Exasol was unavailable on the sandbox).
-- Exasol 7.1 SQL standard syntax. Same column types as PostgreSQL (DECIMAL, VARCHAR, DATE).
-- If Exasol becomes available later, this schema is the one to use.

DROP TABLE IF EXISTS lineitem;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS customer;
DROP TABLE IF EXISTS part;
DROP TABLE IF EXISTS partsupp;
DROP TABLE IF EXISTS supplier;
DROP TABLE IF EXISTS nation;
DROP TABLE IF EXISTS region;

CREATE TABLE region  (
    r_regionkey  DECIMAL(18,0),
    r_name       VARCHAR(25),
    r_comment    VARCHAR(152)
);

CREATE TABLE nation  (
    n_nationkey  DECIMAL(18,0),
    n_name       VARCHAR(25),
    n_regionkey  DECIMAL(18,0),
    n_comment    VARCHAR(152)
);

CREATE TABLE part  (
    p_partkey     DECIMAL(18,0),
    p_name        VARCHAR(55),
    p_mfgr        VARCHAR(25),
    p_brand       VARCHAR(10),
    p_type        VARCHAR(25),
    p_size        DECIMAL(10,0),
    p_container   VARCHAR(10),
    p_retailprice DECIMAL(15,2),
    p_comment     VARCHAR(23)
);

CREATE TABLE supplier  (
    s_suppkey     DECIMAL(18,0),
    s_name        VARCHAR(25),
    s_address     VARCHAR(40),
    s_nationkey   DECIMAL(18,0),
    s_phone       VARCHAR(15),
    s_acctbal     DECIMAL(15,2),
    s_comment     VARCHAR(101)
);

CREATE TABLE partsupp  (
    ps_partkey     DECIMAL(18,0),
    ps_suppkey     DECIMAL(18,0),
    ps_availqty    DECIMAL(10,0),
    ps_supplycost  DECIMAL(15,2),
    ps_comment     VARCHAR(199)
);

CREATE TABLE customer  (
    c_custkey      DECIMAL(18,0),
    c_name         VARCHAR(25),
    c_address      VARCHAR(40),
    c_nationkey    DECIMAL(18,0),
    c_phone        VARCHAR(15),
    c_acctbal      DECIMAL(15,2),
    c_mktsegment   VARCHAR(10),
    c_comment      VARCHAR(117)
);

CREATE TABLE orders  (
    o_orderkey       DECIMAL(18,0),
    o_custkey        DECIMAL(18,0),
    o_orderstatus    VARCHAR(1),
    o_totalprice     DECIMAL(15,2),
    o_orderdate      DATE,
    o_orderpriority  VARCHAR(15),
    o_clerk          VARCHAR(15),
    o_shippriority   DECIMAL(10,0),
    o_comment        VARCHAR(79)
);

CREATE TABLE lineitem  (
    l_orderkey        DECIMAL(18,0),
    l_partkey         DECIMAL(18,0),
    l_suppkey         DECIMAL(18,0),
    l_linenumber      DECIMAL(10,0),
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
