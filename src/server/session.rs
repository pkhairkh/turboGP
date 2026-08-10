//! Per-connection session state.

use std::collections::HashMap;

/// Transaction status reported in ReadyForQuery ('Z').
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TxnStatus {
    #[default]
    Idle, // 'I'
    InTransaction,     // 'T'
    FailedTransaction, // 'E'
}

impl TxnStatus {
    pub fn tag(self) -> u8 {
        match self {
            TxnStatus::Idle => b'I',
            TxnStatus::InTransaction => b'T',
            TxnStatus::FailedTransaction => b'E',
        }
    }
}

/// Per-connection session.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub txn: TxnStatus,
    pub session_context: HashMap<String, String>,
    pub application_name: Option<String>,
    pub user: Option<String>,
    pub database: Option<String>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset_txn(&mut self) {
        self.txn = TxnStatus::Idle;
    }

    pub fn set_context(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.session_context.insert(k.into().to_lowercase(), v.into());
    }
    pub fn get_context(&self, k: &str) -> Option<&str> {
        self.session_context.get(&k.to_lowercase()).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn txn_tags() {
        assert_eq!(TxnStatus::Idle.tag(), b'I');
        assert_eq!(TxnStatus::InTransaction.tag(), b'T');
        assert_eq!(TxnStatus::FailedTransaction.tag(), b'E');
    }
    #[test]
    fn session_context_case_insensitive() {
        let mut s = Session::new();
        s.set_context("UserID", "42");
        assert_eq!(s.get_context("userid"), Some("42"));
        assert_eq!(s.get_context("USERID"), Some("42"));
    }
}
