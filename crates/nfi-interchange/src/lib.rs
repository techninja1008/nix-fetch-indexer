//! Interchange bundle format for nix-fetch-indexer.
//!
//! A [`Bundle`] carries a release table and up to five dataset sections
//! (closure, packages, outpaths, programs, eval-errors). Every entry records
//! the releases (or a release's closures or evaluations) it is present in as
//! a [`Bitset`], so one bundle can hold the results of many releases and
//! still be split per release.

pub mod bitset;
pub mod codec;
pub mod model;
pub mod nixbase32;
pub mod ops;
pub mod store_path;

pub use bitset::Bitset;
pub use codec::{Compression, Format};
pub use model::*;
pub use ops::{ClosureBuilder, EvalErrorsBuilder, PathInfo};
pub use store_path::{StoreHash, StorePath};
