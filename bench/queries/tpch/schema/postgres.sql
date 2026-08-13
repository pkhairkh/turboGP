-- TPC-H schema for PostgreSQL (fallback for Exasol).
-- Standard TPC-H schema from dss.ddl; types use PostgreSQL-native names.
-- Source: electrum/tpch-dbgen/dss.ddl

DROP TABLE IF EXISTS lineitem CASCADE;
DROP TABLE IF EXISTS orders CASCADE;
DROP TABLE IF EXISTS customer CASCADE;
DROP TABLE IF EXISTS part CASCADE;
DROP TABLE IF EXISTS partsupp CASCADE;
DROP TABLE IF EXISTS supplier CASCADE;
DROP TABLE IF EXISTS nation CASCADE;
DROP TABLE IF EXISTS region CASCADE;

CREATE TABLE region  (
    r_regionkey  BIGINT NOT NULL,
    r_name       CHAR(25) NOT NULL,
    r_comment    VARCHAR(152)
);

CREATE TABLE nation  (
    n_nationkey  BIGINT NOT NULL,
    n_name       CHAR(25) NOT NULL,
    n_regionkey  BIGINT NOT NULL,
    n_comment    VARCHAR(152)
);

CREATE TABLE part  (
    p_partkey     BIGINT NOT NULL,
    p_name        VARCHAR(55) NOT NULL,
    p_mfgr        CHAR(25) NOT NULL,
    p_brand       CHAR(10) NOT NULL,
    p_type        VARCHAR(25) NOT NULL,
    p_size        INTEGER NOT NULL,
    p_container   CHAR(10) NOT NULL,
    p_retailprice DECIMAL(15,2) NOT NULL,
    p_comment     VARCHAR(23) NOT NULL
);

CREATE TABLE supplier  (
    s_suppkey     BIGINT NOT NULL,
    s_name        CHAR(25) NOT NULL,
    s_address     VARCHAR(40) NOT NULL,
    s_nationkey   BIGINT NOT NULL,
    s_phone       CHAR(15) NOT NULL,
    s_acctbal     DECIMAL(15,2) NOT NULL,
    s_comment     VARCHAR(101) NOT NULL
);

CREATE TABLE partsupp  (
    ps_partkey     BIGINT NOT NULL,
    ps_suppkey     BIGINT NOT NULL,
    ps_availqty    INTEGER NOT NULL,
    ps_supplycost  DECIMAL(15,2) NOT NULL,
    ps_comment     VARCHAR(199) NOT NULL
);

CREATE TABLE customer  (
    c_custkey      BIGINT NOT NULL,
    c_name         VARCHAR(25) NOT NULL,
    c_address      VARCHAR(40) NOT NULL,
    c_nationkey    BIGINT NOT NULL,
    c_phone        CHAR(15) NOT NULL,
    c_acctbal      DECIMAL(15,2) NOT NULL,
    c_mktsegment   CHAR(10) NOT NULL,
    c_comment      VARCHAR(117) NOT NULL
);

CREATE TABLE orders  (
    o_orderkey       BIGINT NOT NULL,
    o_custkey        BIGINT NOT NULL,
    o_orderstatus    CHAR(1) NOT NULL,
    o_totalprice     DECIMAL(15,2) NOT NULL,
    o_orderdate      DATE NOT NULL,
    o_orderpriority  CHAR(15) NOT NULL,
    o_clerk          CHAR(15) NOT NULL,
    o_shippriority   INTEGER NOT NULL,
    o_comment        VARCHAR(79) NOT NULL
);

CREATE TABLE lineitem  (
    l_orderkey        BIGINT NOT NULL,
    l_partkey         BIGINT NOT NULL,
    l_suppkey         BIGINT NOT NULL,
    l_linenumber      INTEGER NOT NULL,
    l_quantity        DECIMAL(15,2) NOT NULL,
    l_extendedprice   DECIMAL(15,2) NOT NULL,
    l_discount        DECIMAL(15,2) NOT NULL,
    l_tax             DECIMAL(15,2) NOT NULL,
    l_returnflag      CHAR(1) NOT NULL,
    l_linestatus      CHAR(1) NOT NULL,
    l_shipdate        DATE NOT NULL,
    l_commitdate      DATE NOT NULL,
    l_receiptdate     DATE NOT NULL,
    l_shipinstruct    CHAR(25) NOT NULL,
    l_shipmode        CHAR(10) NOT NULL,
    l_comment         VARCHAR(44) NOT NULL
);

-- Primary keys (from dss.ri)
ALTER TABLE region  ADD CONSTRAINT pk_region  PRIMARY KEY (r_regionkey);
ALTER TABLE nation  ADD CONSTRAINT pk_nation  PRIMARY KEY (n_nationkey);
ALTER TABLE part    ADD CONSTRAINT pk_part    PRIMARY KEY (p_partkey);
ALTER TABLE supplier ADD CONSTRAINT pk_supplier PRIMARY KEY (s_suppkey);
ALTER TABLE partsupp ADD CONSTRAINT pk_partsupp PRIMARY KEY (ps_partkey, ps_suppkey);
ALTER TABLE customer ADD CONSTRAINT pk_customer PRIMARY KEY (c_custkey);
ALTER TABLE orders  ADD CONSTRAINT pk_orders  PRIMARY KEY (o_orderkey);
ALTER TABLE lineitem ADD CONSTRAINT pk_lineitem PRIMARY KEY (l_orderkey, l_linenumber);

-- Foreign keys omitted to avoid load overhead. Row counts and result correctness
-- do not depend on FK constraints, and most TPC-H benchmark databases (DuckDB,
-- ClickHouse) don't enforce FKs anyway. Documented in FAIRNESS_AUDIT.md.
