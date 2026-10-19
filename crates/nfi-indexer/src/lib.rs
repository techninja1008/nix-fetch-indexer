//! Indexers for NixOS release artifacts.

pub mod closure;
pub mod fetch;
pub mod index;
pub mod narinfo;
pub mod outpaths;
pub mod packages;
pub mod probes;
pub mod programs;
pub mod release;
pub mod tarball;

#[cfg(test)]
mod e2e_tests;
