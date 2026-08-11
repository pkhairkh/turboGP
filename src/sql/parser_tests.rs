    use super::*;
    use crate::sql::lexer::tokenize;

    /// Helper: tokenize + parse in one step.
    fn parse_sql(s: &str) -> Result<SelectQuery, String> {
        parse(tokenize(s)?)
    }

    // ===== Orchestrator Return Criteria Verification =====
    // These tests verify the specific SQL examples from the Orchestrator
    // Return Criteria section of the Wave 8 spec.

    #[test]
    fn return_crit_in_subquery() {
        assert!(parse_sql("SELECT * FROM t WHERE id IN (SELECT id FROM t2)").is_ok());
    }

    #[test]
    fn return_crit_union() {
        let tokens = tokenize("SELECT a FROM t1 UNION SELECT a FROM t2").unwrap();
        assert!(parse_set(tokens).is_ok());
    }

    #[test]
    fn return_crit_qualified_column() {
        assert!(parse_sql("SELECT t.col FROM t").is_ok());
    }

    #[test]
    fn return_crit_is_null() {
        assert!(parse_sql("SELECT * FROM t WHERE x IS NULL").is_ok());
    }

    #[test]
    fn return_crit_unary_minus() {
        assert!(parse_sql("SELECT -1 FROM t").is_ok());
    }

    #[test]
    fn return_crit_scalar_function() {
        assert!(parse_sql("SELECT UPPER(name) FROM t").is_ok());
    }

    #[test]
    fn return_crit_cte() {
        let sql = "WITH t AS (SELECT * FROM x) SELECT * FROM t";
        let result = crate::sql::cte::parse_with(sql);
        assert!(result.is_some());
        assert!(result.unwrap().is_ok());
    }

    #[test]
    fn return_crit_insert_select() {
        let result = crate::sql::dml::parse_dml("INSERT INTO t SELECT * FROM t2");
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn return_crit_check_constraint() {
        let result = crate::sql::ddl::parse_ddl("CREATE TABLE t (x INT CHECK (x > 0))");
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn return_crit_multi_column_index() {
        let result = crate::sql::ddl::parse_ddl("CREATE INDEX idx ON t (a, b)");
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn parse_select_star_with_where() {
        let q = parse_sql("SELECT * FROM t WHERE x = 5").unwrap();
        assert_eq!(q.select.len(), 1);
        assert!(matches!(q.select[0], SelectItem::Star));
        assert_eq!(q.from, "t");
        let w = q.where_clause.expect("WHERE clause");
        match w {
            Expr::Binary { left, op, right } => {
                assert!(op == BinOp::Eq);
                match *left {
                    Expr::Column(c) => assert_eq!(c, "x"),
                    other => panic!("expected Column, got {other:?}"),
                }
                match *right {
                    Expr::Literal(Value::Int(5)) => {}
                    other => panic!("expected Int(5), got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_star() {
        let q = parse_sql("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "COUNT");
                assert_eq!(arg, "*");
                assert_eq!(*alias, None);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
        assert_eq!(q.from, "t");
        assert!(q.where_clause.is_none());
    }

    #[test]
    fn parse_sum_with_alias() {
        let q = parse_sql("SELECT SUM(price) AS total FROM sales").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "SUM");
                assert_eq!(arg, "price");
                assert_eq!(*alias, Some("total".to_string()));
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_avg_group_by() {
        let q = parse_sql("SELECT AVG(price) FROM sales GROUP BY area").unwrap();
        assert_eq!(q.group_by, vec!["area"]);
    }

    #[test]
    fn parse_order_by_asc_desc() {
        let q = parse_sql("SELECT * FROM t ORDER BY a ASC, b DESC, c").unwrap();
        assert_eq!(q.order_by.len(), 3);
        assert_eq!(q.order_by[0].0, "a"); assert!(q.order_by[0].1);
        assert_eq!(q.order_by[1].0, "b"); assert!(!q.order_by[1].1);
        assert_eq!(q.order_by[2].0, "c"); assert!(q.order_by[2].1);
    }

    #[test]
    fn parse_limit() {
        let q = parse_sql("SELECT * FROM t LIMIT 100").unwrap();
        assert_eq!(q.limit, Some(100));
    }

    #[test]
    fn parse_multiple_columns() {
        let q = parse_sql("SELECT a, b, c FROM t").unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Column(c) if c == "a"));
        assert!(matches!(&q.select[1], SelectItem::Column(c) if c == "b"));
        assert!(matches!(&q.select[2], SelectItem::Column(c) if c == "c"));
    }

    #[test]
    fn parse_and_or_precedence() {
        // a = 1 AND b = 2 OR c = 3  →  (a=1 AND b=2) OR (c=3)
        let q = parse_sql("SELECT * FROM t WHERE a = 1 AND b = 2 OR c = 3").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, right } => {
                assert!(op == BinOp::Or);
                // Left should be AND.
                match *left {
                    Expr::Binary { op, .. } => { assert!(op == BinOp::And) }
                    other => panic!("expected AND, got {other:?}"),
                }
                // Right should be a comparison.
                match *right {
                    Expr::Binary { op, .. } => { assert!(op == BinOp::Eq) }
                    other => panic!("expected =, got {other:?}"),
                }
            }
            other => panic!("expected OR at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_arithmetic_precedence() {
        // a + b * c  →  a + (b * c)
        let q = parse_sql("SELECT * FROM t WHERE x = a + b * c").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op: op_eq, right } => {
                assert!(op_eq == BinOp::Eq);
                assert!(matches!(*left, Expr::Column(_)));
                match *right {
                    Expr::Binary { op, right: mul_right, .. } => {
                        assert!(op == BinOp::Add);
                        match *mul_right {
                            Expr::Binary { op, .. } => { assert!(op == BinOp::Mul) }
                            other => panic!("expected *, got {other:?}"),
                        }
                    }
                    other => panic!("expected +, got {other:?}"),
                }
            }
            other => panic!("expected =, got {other:?}"),
        }
    }

    #[test]
    fn parse_parenthesized_expr() {
        let q = parse_sql("SELECT * FROM t WHERE (a = 1 OR b = 2) AND c = 3").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, .. } => {
                assert!(op == BinOp::And);
                // The left side is now wrapped in Expr::Paren since Wave 2
                // preserves the source grouping for AST fidelity.
                match *left {
                    Expr::Paren(inner) => match *inner {
                        Expr::Binary { op, .. } => { assert!(op == BinOp::Or) }
                        other => panic!("expected OR inside Paren, got {other:?}"),
                    },
                    other => panic!("expected Paren, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_is_null() {
        let q = parse_sql("SELECT * FROM t WHERE x IS NULL").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::IsNull { expr, negated } => {
                assert!(!negated);
                match *expr {
                    Expr::Column(c) => assert_eq!(c, "x"),
                    other => panic!("expected Column(x), got {other:?}"),
                }
            }
            other => panic!("expected IsNull, got {other:?}"),
        }
    }

    #[test]
    fn parse_is_not_null() {
        let q = parse_sql("SELECT * FROM t WHERE y IS NOT NULL").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::IsNull { negated, .. } => assert!(negated),
            other => panic!("expected IsNull, got {other:?}"),
        }
    }

    #[test]
    fn parse_is_null_with_and() {
        // x IS NULL AND y > 5 — verify precedence
        let q = parse_sql("SELECT * FROM t WHERE x IS NULL AND y > 5").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, .. } => {
                assert!(op == BinOp::And);
                match *left {
                    Expr::IsNull { .. } => {}
                    other => panic!("expected IsNull on left, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_not_prefix() {
        // NOT (x > 5)
        let q = parse_sql("SELECT * FROM t WHERE NOT (x > 5)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Not(inner) => match *inner {
                Expr::Paren(p) => match *p {
                    Expr::Binary { op, .. } => assert!(op == BinOp::Gt),
                    other => panic!("expected Binary inside Paren, got {other:?}"),
                },
                other => panic!("expected Paren inside Not, got {other:?}"),
            },
            other => panic!("expected Not, got {other:?}"),
        }
    }

    #[test]
    fn parse_not_with_and() {
        // NOT a = 1 AND b = 2 — should parse as (NOT (a = 1)) AND (b = 2)
        let q = parse_sql("SELECT * FROM t WHERE NOT a = 1 AND b = 2").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, .. } => {
                assert!(op == BinOp::And);
                match *left {
                    Expr::Not(_) => {}
                    other => panic!("expected Not on left, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_unary_minus_literal() {
        // SELECT -1 FROM t — unary minus on a literal
        let q = parse_sql("SELECT * FROM t WHERE x = -1").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Unary { op, expr } => {
                        assert!(op == UnaryOp::Neg);
                        match *expr {
                            Expr::Literal(Value::Int(1)) => {}
                            other => panic!("expected Int(1), got {other:?}"),
                        }
                    }
                    other => panic!("expected Unary, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_unary_minus_paren() {
        // -(a + b)
        let q = parse_sql("SELECT * FROM t WHERE x = -(a + b)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Unary { op: UnaryOp::Neg, expr } => {
                        match *expr {
                            Expr::Paren(_) => {}
                            other => panic!("expected Paren inside Unary, got {other:?}"),
                        }
                    }
                    other => panic!("expected Unary, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_qualified_column_in_select() {
        let q = parse_sql("SELECT t.col FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Column(name) => assert_eq!(name, "t.col"),
            other => panic!("expected Column(t.col), got {other:?}"),
        }
    }

    #[test]
    fn parse_qualified_column_in_where() {
        let q = parse_sql("SELECT * FROM t WHERE t1.id = t2.id").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, right } => {
                assert!(op == BinOp::Eq);
                match *left {
                    Expr::Column(c) => assert_eq!(c, "t1.id"),
                    other => panic!("expected Column(t1.id), got {other:?}"),
                }
                match *right {
                    Expr::Column(c) => assert_eq!(c, "t2.id"),
                    other => panic!("expected Column(t2.id), got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_function_upper() {
        // UPPER(name) — single-arg scalar function
        let q = parse_sql("SELECT * FROM t WHERE x = UPPER(name)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, args, distinct } => {
                        assert_eq!(name, "UPPER");
                        assert!(!distinct);
                        assert_eq!(args.len(), 1);
                        match &args[0] {
                            Expr::Column(c) => assert_eq!(c, "name"),
                            other => panic!("expected Column(name), got {other:?}"),
                        }
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_function_substr_multi_arg() {
        // SUBSTR(name, 1, 3) — three-arg scalar function
        let q = parse_sql("SELECT * FROM t WHERE x = SUBSTR(name, 1, 3)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, args, .. } => {
                        assert_eq!(name, "SUBSTR");
                        assert_eq!(args.len(), 3);
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_function_coalesce_n_args() {
        // COALESCE(a, b, c) — N-arg function
        let q = parse_sql("SELECT * FROM t WHERE x = COALESCE(a, b, c)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, args, .. } => {
                        assert_eq!(name, "COALESCE");
                        assert_eq!(args.len(), 3);
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_function_case_insensitive() {
        // Function names are case-insensitive — lowercased should still uppercase
        let q = parse_sql("SELECT * FROM t WHERE x = upper(name)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, .. } => assert_eq!(name, "UPPER"),
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_combined_wave3_features() {
        // WHERE NOT (x > 5) AND y IS NOT NULL — combines NOT, paren, IS NOT NULL, AND
        let q = parse_sql("SELECT * FROM t WHERE NOT (x > 5) AND y IS NOT NULL").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, right } => {
                assert!(op == BinOp::And);
                assert!(matches!(*left, Expr::Not(_)));
                match *right {
                    Expr::IsNull { negated, .. } => assert!(negated),
                    other => panic!("expected IsNull on right, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    // ===== Wave 4: Subqueries and Set Operations =====

    #[test]
    fn parse_scalar_subquery_in_select() {
        // SELECT (SELECT COUNT(*) FROM t2) FROM t
        let q = parse_sql("SELECT (SELECT COUNT(*) FROM t2) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Function { name, args, .. } => {
                    assert_eq!(name, "__scalar_subquery__");
                    assert_eq!(args.len(), 1);
                    match &args[0] {
                        Expr::Literal(Value::String(sql)) => {
                            assert!(sql.contains("SELECT"), "subquery SQL: {sql}");
                            assert!(sql.contains("COUNT"));
                        }
                        other => panic!("expected String literal, got {other:?}"),
                    }
                }
                other => panic!("expected Function (scalar subquery), got {other:?}"),
            },
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_subquery_in_where() {
        // WHERE x = (SELECT MAX(y) FROM t2)
        let q = parse_sql("SELECT * FROM t WHERE x = (SELECT MAX(y) FROM t2)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, .. } => {
                        assert_eq!(name, "__scalar_subquery__");
                    }
                    other => panic!("expected scalar subquery, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_in_subquery() {
        // WHERE id IN (SELECT id FROM t2)
        let q = parse_sql("SELECT * FROM t WHERE id IN (SELECT id FROM t2)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::InSubquery { expr, subquery_sql, negated } => {
                assert!(!negated);
                assert!(subquery_sql.contains("SELECT"));
                match *expr {
                    Expr::Column(c) => assert_eq!(c, "id"),
                    other => panic!("expected Column(id), got {other:?}"),
                }
            }
            other => panic!("expected InSubquery, got {other:?}"),
        }
    }

    #[test]
    fn parse_not_in_subquery() {
        let q = parse_sql("SELECT * FROM t WHERE id NOT IN (SELECT id FROM t2)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::InSubquery { negated, .. } => assert!(negated),
            other => panic!("expected InSubquery, got {other:?}"),
        }
    }

    #[test]
    fn parse_in_list_still_works() {
        // IN (1, 2, 3) should still produce InList, not InSubquery
        let q = parse_sql("SELECT * FROM t WHERE id IN (1, 2, 3)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::InList { list, negated, .. } => {
                assert!(!negated);
                assert_eq!(list.len(), 3);
            }
            other => panic!("expected InList, got {other:?}"),
        }
    }

    #[test]
    fn parse_exists_subquery() {
        // WHERE EXISTS (SELECT * FROM t2 WHERE t2.id = t.id)
        let q = parse_sql("SELECT * FROM t WHERE EXISTS (SELECT * FROM t2 WHERE t2.id = t.id)")
            .unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Exists { subquery_sql, negated } => {
                assert!(!negated);
                assert!(subquery_sql.contains("SELECT"));
                assert!(subquery_sql.contains("t2"));
            }
            other => panic!("expected Exists, got {other:?}"),
        }
    }

    #[test]
    fn parse_not_exists_subquery() {
        let q = parse_sql("SELECT * FROM t WHERE NOT EXISTS (SELECT * FROM t2)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Exists { negated, .. } => assert!(negated),
            other => panic!("expected Exists, got {other:?}"),
        }
    }

    #[test]
    fn parse_union() {
        let tokens = crate::sql::lexer::tokenize("SELECT a FROM t1 UNION SELECT a FROM t2").unwrap();
        let set = parse_set(tokens).unwrap();
        match set {
            SetQuery::Union(left, right) => {
                assert!(matches!(*left, SetQuery::Select(_)));
                assert!(matches!(*right, SetQuery::Select(_)));
            }
            other => panic!("expected Union, got {other:?}"),
        }
    }

    #[test]
    fn parse_union_all() {
        let tokens =
            crate::sql::lexer::tokenize("SELECT a FROM t1 UNION ALL SELECT a FROM t2").unwrap();
        let set = parse_set(tokens).unwrap();
        assert!(matches!(set, SetQuery::UnionAll(_, _)));
    }

    #[test]
    fn parse_intersect() {
        let tokens =
            crate::sql::lexer::tokenize("SELECT a FROM t1 INTERSECT SELECT a FROM t2").unwrap();
        let set = parse_set(tokens).unwrap();
        assert!(matches!(set, SetQuery::Intersect(_, _)));
    }

    #[test]
    fn parse_except() {
        let tokens =
            crate::sql::lexer::tokenize("SELECT a FROM t1 EXCEPT SELECT a FROM t2").unwrap();
        let set = parse_set(tokens).unwrap();
        assert!(matches!(set, SetQuery::Except(_, _)));
    }

    #[test]
    fn parse_set_precedence_intersect_over_union() {
        // SELECT a FROM t1 UNION SELECT a FROM t2 INTERSECT SELECT a FROM t3
        // should parse as t1 UNION (t2 INTERSECT t3) because INTERSECT
        // binds tighter than UNION.
        let tokens = crate::sql::lexer::tokenize(
            "SELECT a FROM t1 UNION SELECT a FROM t2 INTERSECT SELECT a FROM t3",
        )
        .unwrap();
        let set = parse_set(tokens).unwrap();
        match set {
            SetQuery::Union(left, right) => {
                assert!(matches!(*left, SetQuery::Select(_)));
                assert!(matches!(*right, SetQuery::Intersect(_, _)));
            }
            other => panic!("expected Union(Select, Intersect), got {other:?}"),
        }
    }

    #[test]
    fn parse_set_parenthesised() {
        // (SELECT a FROM t1 UNION SELECT a FROM t2) ORDER BY a
        // The parenthesised body is one operand.
        let tokens = crate::sql::lexer::tokenize(
            "(SELECT a FROM t1 UNION SELECT a FROM t2)",
        )
        .unwrap();
        let set = parse_set(tokens).unwrap();
        match set {
            SetQuery::Union(_, _) => {}
            other => panic!("expected Union, got {other:?}"),
        }
    }

    #[test]
    fn parse_set_backcompat_single_select() {
        // parse() (not parse_set()) should still return a single SelectQuery
        // for a non-set-operation query.
        let q = parse_sql("SELECT * FROM t WHERE x = 5").unwrap();
        assert_eq!(q.from, "t");
    }

    // ===== Wave 8: OFFSET/FETCH, NULLS FIRST/LAST, DISTINCT ON =====

    #[test]
    fn parse_offset_with_limit() {
        let q = parse_sql("SELECT * FROM t LIMIT 10 OFFSET 20").unwrap();
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.offset, Some(20));
    }

    #[test]
    fn parse_offset_only() {
        let q = parse_sql("SELECT * FROM t OFFSET 20 ROWS").unwrap();
        assert_eq!(q.offset, Some(20));
        assert!(q.limit.is_none());
    }

    #[test]
    fn parse_offset_fetch() {
        let q = parse_sql("SELECT * FROM t OFFSET 20 ROWS FETCH FIRST 10 ROWS ONLY").unwrap();
        assert_eq!(q.offset, Some(20));
        assert_eq!(q.fetch, Some(10));
    }

    #[test]
    fn parse_fetch_next() {
        // FETCH NEXT n ROWS ONLY (alternative to FIRST)
        let q = parse_sql("SELECT * FROM t FETCH NEXT 5 ROWS ONLY").unwrap();
        assert_eq!(q.fetch, Some(5));
    }

    #[test]
    fn parse_nulls_first() {
        let q = parse_sql("SELECT * FROM t ORDER BY x ASC NULLS FIRST").unwrap();
        assert_eq!(q.order_by.len(), 1);
        assert_eq!(q.order_by[0].0, "x");
        assert!(q.order_by[0].1, "ascending");
        assert_eq!(q.order_by[0].2, NullsOrder::First);
    }

    #[test]
    fn parse_nulls_last() {
        let q = parse_sql("SELECT * FROM t ORDER BY x DESC NULLS LAST").unwrap();
        assert_eq!(q.order_by.len(), 1);
        assert!(!q.order_by[0].1, "descending");
        assert_eq!(q.order_by[0].2, NullsOrder::Last);
    }

    #[test]
    fn parse_nulls_default() {
        // No NULLS clause -> Default
        let q = parse_sql("SELECT * FROM t ORDER BY x").unwrap();
        assert_eq!(q.order_by[0].2, NullsOrder::Default);
    }

    #[test]
    fn parse_distinct_on() {
        let q = parse_sql("SELECT DISTINCT ON (a) a, b FROM t ORDER BY a, b").unwrap();
        assert!(q.distinct);
        assert_eq!(q.distinct_on, Some(vec!["a".to_string()]));
    }

    #[test]
    fn parse_distinct_on_multi_column() {
        let q = parse_sql("SELECT DISTINCT ON (a, b) a, b, c FROM t").unwrap();
        assert!(q.distinct);
        assert_eq!(q.distinct_on, Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn parse_distinct_without_on_still_works() {
        // SELECT DISTINCT col — no ON clause, distinct_on should be None
        let q = parse_sql("SELECT DISTINCT col FROM t").unwrap();
        assert!(q.distinct);
        assert!(q.distinct_on.is_none());
    }

    #[test]
    fn parse_trailing_semicolon_ok() {
        let q = parse_sql("SELECT * FROM t;").unwrap();
        assert_eq!(q.from, "t");
    }

    #[test]
    fn parse_string_literal_in_where() {
        let q = parse_sql("SELECT * FROM t WHERE name = 'alice'").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, .. } => match *right {
                Expr::Literal(Value::String(s)) => assert_eq!(s, "alice"),
                other => panic!("expected String, got {other:?}"),
            },
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_invalid_missing_select_list() {
        let r = parse_sql("SELECT FROM WHERE");
        assert!(r.is_err(), "expected error for SELECT FROM WHERE");
    }

    #[test]
    fn parse_invalid_missing_table() {
        let r = parse_sql("SELECT * FROM WHERE");
        assert!(r.is_err(), "expected error for missing table name");
    }

    #[test]
    fn parse_invalid_negative_limit() {
        let r = parse_sql("SELECT * FROM t LIMIT -5");
        assert!(r.is_err(), "expected error for negative LIMIT");
    }

    #[test]
    fn parse_invalid_unexpected_eof() {
        // FROM is now optional (Wave 6), so "SELECT *" should parse
        // successfully and use the __dummy__ table.
        let r = parse_sql("SELECT *");
        assert!(r.is_ok(), "SELECT * without FROM should parse, got: {r:?}");
    }

    #[test]
    fn parse_invalid_trailing_garbage() {
        let r = parse_sql("SELECT * FROM t WHERE x = 5 garbage");
        assert!(r.is_err(), "expected error for trailing garbage");
    }

    #[test]
    fn parse_count_distinct_keyword() {
        // `COUNT(DISTINCT col)` is normalised to
        // `Aggregate { func: "COUNT_DISTINCT", arg: "col" }`.
        let q = parse_sql("SELECT COUNT(DISTINCT user_id) FROM events").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "COUNT_DISTINCT");
                assert_eq!(arg, "user_id");
                assert_eq!(*alias, None);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_distinct_case_insensitive() {
        // `count(distinct col)` should normalise the same way.
        let q = parse_sql("SELECT count(distinct x) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, .. } => {
                assert_eq!(func, "COUNT_DISTINCT");
                assert_eq!(arg, "x");
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_distinct_requires_column() {
        // `COUNT(DISTINCT)` with no column should error.
        let r = parse_sql("SELECT COUNT(DISTINCT) FROM t");
        assert!(r.is_err(), "expected error for COUNT(DISTINCT) without column");
    }

    #[test]
    fn parse_sum_distinct_keyword() {
        // `SUM(DISTINCT col)` works the same way (produces SUM_DISTINCT).
        let q = parse_sql("SELECT SUM(DISTINCT price) FROM sales").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, .. } => {
                assert_eq!(func, "SUM_DISTINCT");
                assert_eq!(arg, "price");
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_select_integer_literal() {
        // `SELECT 1, URL, count(*)` — ClickBench Q15-Q42 shape.
        let q = parse_sql("SELECT 1, URL, count(*) FROM t").unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Literal(1)));
        assert!(matches!(&q.select[1], SelectItem::Column(c) if c == "URL"));
        assert!(
            matches!(&q.select[2], SelectItem::Aggregate { func, arg, .. } if func == "COUNT" && arg == "*")
        );
    }

    #[test]
    fn parse_group_by_positional_and_column() {
        // `GROUP BY 1, URL` — the positional `1` is skipped, only URL
        // remains as a real GROUP BY key.
        let q = parse_sql("SELECT 1, URL, count(*) FROM t GROUP BY 1, URL").unwrap();
        assert_eq!(q.group_by, vec!["URL"]);
    }

    #[test]
    fn parse_group_by_positional_only() {
        // `GROUP BY 1` alone (degenerate but legal) → empty group_by.
        let q = parse_sql("SELECT 1, count(*) FROM t GROUP BY 1").unwrap();
        assert!(q.group_by.is_empty());
    }

    #[test]
    fn parse_select_negative_literal_now_supported() {
        // Wave 3: SELECT -1 now parses (previously rejected) thanks to
        // unary minus support in the SELECT list.
        let q = parse_sql("SELECT -1 FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Unary { op: UnaryOp::Neg, expr: inner } => match inner.as_ref() {
                    Expr::Literal(Value::Int(1)) => {}
                    other => panic!("expected Int(1) inside Unary, got {other:?}"),
                },
                other => panic!("expected Unary(Neg, Int(1)), got {other:?}"),
            },
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    #[test]
    fn parse_clickbench_q15_shape() {
        // Full Q15 shape: SELECT 1, URL, count(*) AS c FROM t WHERE URL LIKE 'https://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10
        let q = parse_sql(
            "SELECT 1, URL, count(*) AS c FROM t WHERE URL LIKE 'https://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10",
        )
        .unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Literal(1)));
        assert!(matches!(&q.select[2], SelectItem::Aggregate { alias: Some(a), .. } if a == "c"));
        assert_eq!(q.group_by, vec!["URL"]);
        assert_eq!(q.order_by[0].0, "c"); assert!(!q.order_by[0].1);
        assert_eq!(q.limit, Some(10));
    }

    /// Wave 62 fix: HAVING with count(*) must parse without error.
    /// Previously, parse_primary didn't handle `IDENT(` as a function call
    /// in expression context, causing "unexpected trailing token: LParen".
    #[test]
    fn parse_having_with_count_star() {
        let q =
            parse_sql("SELECT dept, count(*) FROM t GROUP BY dept HAVING count(*) > 1").unwrap();
        assert!(q.having.is_some(), "HAVING clause must be parsed");
        // Verify the HAVING expression is a Binary comparison.
        match &q.having {
            Some(Expr::Binary { left, op, right }) => {
                assert!(*op == BinOp::Gt);
                // Left should be Expr::Function { name: "COUNT", args: [Wildcard] }
                match left.as_ref() {
                    Expr::Function { name, args, .. } => {
                        assert_eq!(name, "COUNT");
                        assert!(args.iter().any(|a| *a == Expr::Wildcard), "args should contain Wildcard");
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
                // Right should be Literal(Int(1))
                match right.as_ref() {
                    Expr::Literal(Value::Int(1)) => {}
                    other => panic!("expected Int(1), got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    /// Wave 62 fix: HAVING with sum(col) must also parse.
    #[test]
    fn parse_having_with_sum() {
        let q = parse_sql("SELECT dept FROM t GROUP BY dept HAVING sum(salary) > 400").unwrap();
        assert!(q.having.is_some());
        match &q.having {
            Some(Expr::Binary { left, op, .. }) => {
                assert!(*op == BinOp::Gt);
                match left.as_ref() {
                    Expr::Function { name, args, .. } => {
                        assert_eq!(name, "SUM");
                        // args[0] should be Column("salary")
                        assert!(args.iter().any(|a| matches!(a, Expr::Column(c) if c == "salary")));
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    /// Wave 60d: SELECT DISTINCT must parse and set the distinct flag.
    #[test]
    fn parse_select_distinct() {
        let q = parse_sql("SELECT DISTINCT dept FROM t").unwrap();
        assert!(q.distinct, "distinct flag must be true");
        assert_eq!(q.select.len(), 1);
    }

    /// SELECT without DISTINCT must have distinct = false.
    #[test]
    fn parse_select_without_distinct() {
        let q = parse_sql("SELECT dept FROM t").unwrap();
        assert!(!q.distinct, "distinct flag must be false");
    }

    /// Wave 60a: CASE WHEN in SELECT list must parse as SelectItem::Expression.
    #[test]
    fn parse_case_when_in_select() {
        let q = parse_sql("SELECT CASE WHEN x > 5 THEN 1 ELSE 0 END FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, alias } => {
                assert!(alias.is_none());
                assert!(matches!(expr, Expr::Case { .. }));
            }
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    /// Wave 67: EXTRACT(YEAR FROM d) must parse to Expr::Extract.
    #[test]
    fn parse_extract_year() {
        let q = parse_sql("SELECT EXTRACT(YEAR FROM d) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, alias } => {
                assert!(alias.is_none());
                match expr {
                    Expr::Extract { field, expr } => {
                        assert_eq!(field, "YEAR", "field must be YEAR (uppercased)");
                        // The inner expr should be a Column("d").
                        match expr.as_ref() {
                            Expr::Column(name) => assert_eq!(name, "d"),
                            other => panic!("expected Column(d), got {other:?}"),
                        }
                    }
                    other => panic!("expected Expr::Extract, got {other:?}"),
                }
            }
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    /// Wave 67: EXTRACT with MONTH and DAY fields also parse.
    #[test]
    fn parse_extract_month_day() {
        let q = parse_sql("SELECT EXTRACT(MONTH FROM d) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Extract { field, .. } => assert_eq!(field, "MONTH"),
                other => panic!("expected Extract, got {other:?}"),
            },
            _ => panic!("expected Expression"),
        }
        let q = parse_sql("SELECT EXTRACT(DAY FROM d) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Extract { field, .. } => assert_eq!(field, "DAY"),
                other => panic!("expected Extract, got {other:?}"),
            },
            _ => panic!("expected Expression"),
        }
    }

    /// Wave 67: EXTRACT is case-insensitive (extract(year from d)).
    #[test]
    fn parse_extract_case_insensitive() {
        let q = parse_sql("SELECT extract(year from d) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Extract { field, .. } => assert_eq!(field, "YEAR"),
                other => panic!("expected Extract, got {other:?}"),
            },
            _ => panic!("expected Expression"),
        }
    }

    /// Wave 67: CAST(x AS INT) must parse to Expr::Cast.
    #[test]
    fn parse_cast_int() {
        let q = parse_sql("SELECT CAST(x AS INT) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, alias } => {
                assert!(alias.is_none());
                match expr {
                    Expr::Cast { expr, target_type } => {
                        assert_eq!(target_type, "INT", "target_type must be INT");
                        match expr.as_ref() {
                            Expr::Column(name) => assert_eq!(name, "x"),
                            other => panic!("expected Column(x), got {other:?}"),
                        }
                    }
                    other => panic!("expected Expr::Cast, got {other:?}"),
                }
            }
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    /// Wave 67: CAST with FLOAT, VARCHAR, BIGINT target types.
    #[test]
    fn parse_cast_other_types() {
        for (sql, expected_type) in [
            ("SELECT CAST(x AS FLOAT) FROM t", "FLOAT"),
            ("SELECT CAST(x AS BIGINT) FROM t", "BIGINT"),
            ("SELECT CAST(x AS VARCHAR) FROM t", "VARCHAR"),
            ("SELECT CAST(x AS VARCHAR(50)) FROM t", "VARCHAR"),
        ] {
            let q = parse_sql(sql).unwrap();
            match &q.select[0] {
                SelectItem::Expression { expr, .. } => match expr {
                    Expr::Cast { target_type, .. } => {
                        assert_eq!(*target_type, expected_type, "SQL: {sql}");
                    }
                    other => panic!("SQL {sql}: expected Cast, got {other:?}"),
                },
                other => panic!("SQL {sql}: expected Expression, got {other:?}"),
            }
        }
    }

    /// Wave 67: CAST is case-insensitive (cast(x as int)).
    #[test]
    fn parse_cast_case_insensitive() {
        let q = parse_sql("SELECT cast(x as int) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Cast { target_type, .. } => assert_eq!(*target_type, "INT"),
                other => panic!("expected Cast, got {other:?}"),
            },
            _ => panic!("expected Expression"),
        }
    }

    /// Wave 67: EXTRACT in WHERE clause must parse (not error).
    #[test]
    fn parse_extract_in_where() {
        let q = parse_sql("SELECT * FROM t WHERE EXTRACT(YEAR FROM d) = 2024").unwrap();
        let w = q.where_clause.expect("WHERE clause");
        match w {
            Expr::Binary { left, op, .. } => {
                assert!(op == BinOp::Eq);
                match *left {
                    Expr::Extract { field, .. } => assert_eq!(field, "YEAR"),
                    other => panic!("expected Extract in WHERE left, got {other:?}"),
                }
            }
            other => panic!("expected Binary in WHERE, got {other:?}"),
        }
    }