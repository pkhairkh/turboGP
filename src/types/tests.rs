//! Tests for the expanded `Error` enum.

use crate::Error;

/// Verify that `Error` variants format correctly.
#[test]
fn error_new_variants_format_correctly() {
    assert_eq!(format!("{}", Error::Tier("data not in CXL".into())), "tier error: data not in CXL");
    assert_eq!(
        format!("{}", Error::Protocol("CXL leaked to Raft txn".into())),
        "protocol error: CXL leaked to Raft txn"
    );
    assert_eq!(
        format!("{}", Error::Parse("unexpected token 'FROM'".into())),
        "parse error: unexpected token 'FROM'"
    );
    assert_eq!(format!("{}", Error::Timeout(5_000)), "timeout after 5000 ms");
}
