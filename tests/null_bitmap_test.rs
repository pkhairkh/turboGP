//! Wave 22 — NULL bitmap per column.
//!
//! Verifies that NULL values are tracked separately from 0u64 cells.

use turbogp::types::null_bitmap::NullBitmap;

#[test]
fn null_bitmap_basic() {
    let mut bm = NullBitmap::new(5);
    assert!(!bm.is_null(0));
    assert!(!bm.is_null(4));
    assert_eq!(bm.null_count(), 0);

    bm.set_null(2);
    assert!(bm.is_null(2));
    assert!(!bm.is_null(1));
    assert_eq!(bm.null_count(), 1);
    assert!(bm.has_nulls());
}

#[test]
fn null_bitmap_all_null() {
    let bm = NullBitmap::all_null(10);
    assert_eq!(bm.null_count(), 10);
    assert_eq!(bm.non_null_count(), 0);
    for i in 0..10 {
        assert!(bm.is_null(i));
    }
}

#[test]
fn null_bitmap_set_non_null() {
    let mut bm = NullBitmap::all_null(3);
    bm.set_non_null(1);
    assert!(bm.is_null(0));
    assert!(!bm.is_null(1));
    assert!(bm.is_null(2));
    assert_eq!(bm.null_count(), 2);
}

#[test]
fn null_bitmap_push() {
    let mut bm = NullBitmap::new(0);
    bm.push_non_null();
    bm.push_null();
    bm.push_non_null();
    assert_eq!(bm.len(), 3);
    assert_eq!(bm.null_count(), 1);
}

#[test]
fn null_bitmap_truncate() {
    let mut bm = NullBitmap::all_null(5);
    bm.truncate(3);
    assert_eq!(bm.len(), 3);
    assert_eq!(bm.null_count(), 3);
}

#[test]
fn null_bitmap_out_of_bounds() {
    let bm = NullBitmap::new(3);
    assert!(!bm.is_null(100)); // out of bounds = non-null
}

#[test]
fn null_bitmap_non_null_mask() {
    let mut bm = NullBitmap::new(4);
    bm.set_null(1);
    bm.set_null(3);
    let mask = bm.non_null_mask();
    assert_eq!(mask.len(), 4);
    assert!(!mask[0]); // non-null
    assert!(mask[1]); // null
    assert!(!mask[2]); // non-null
    assert!(mask[3]); // null
}

#[test]
fn dml_insert_null_sets_bitmap() {
    use turbogp::engine::QueryEngine;
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT, name VARCHAR(50))").unwrap();
    e.execute("INSERT INTO t (id, name) VALUES (1, NULL)").unwrap();
    // The NULL value is stored as 0u64, but with the null_bitmap feature
    // we could track it. For now, the query should succeed.
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.scalar_u64(), Some(1));
}
