//! # Index — secondary indexes and approximate search structures.
//!
//! This module groups the secondary indexes that sit on top of the
//! instruction-shaped storage layer:
//!
//! - [`bsi`] — Bit-Sliced Index. 64 bitmaps, one per bit position of the
//!   64-bit cell. Equality is a 64-way AND/OR over bit slices (no ADR — see
//!   `src/index/bsi.rs`).
//! - [`lsh`] — Locality-Sensitive Hash index for approximate nearest
//!   neighbour search over floating-point vectors (ADR-017).
//!
//! Both structures trade exactness (LSH) or compile-time generality (BSI is
//! u64-only) for raw instruction throughput: every primitive operation is
//! a bitwise SIMD-friendly op over a contiguous bit-vector.

pub mod manager;
