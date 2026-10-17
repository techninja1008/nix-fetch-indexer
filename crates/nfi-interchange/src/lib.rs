//! Interchange bundle format for nix-fetch-indexer.
//!
//! A [`Bundle`] carries a release table and up to four dataset sections
//! (closure, packages, outpaths, programs). Every entry records the releases
//! it is present in as a [`Bitset`] over the release table, so one bundle can
//! hold the results of many releases and still be split per release.

pub mod bitset;
pub mod codec;
pub mod model;
pub mod nixbase32;
pub mod ops;
pub mod store_path;

pub use bitset::Bitset;
pub use codec::{Compression, Format};
pub use model::*;
pub use ops::{ClosureBuilder, PathInfo};
pub use store_path::{StoreHash, StorePath};
