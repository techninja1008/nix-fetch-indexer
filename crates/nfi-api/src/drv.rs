//! Reduced derivation graphs: the derivations of a build, reduced to the
//! parts needed for dependency planning (see the `nfi-drv` crate).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A derivation reduced to the parts needed for dependency planning.
/// All paths are full `/nix/store/...` paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReducedDrv {
    /// Output name to path (`None` for content-addressed/floating outputs).
    pub outputs: BTreeMap<String, Option<String>>,
    /// Input derivation to the output names consumed from it.
    #[serde(default, rename = "inputDrvs")]
    pub input_drvs: BTreeMap<String, BTreeSet<String>>,
    #[serde(default, rename = "inputSrcs")]
    pub input_srcs: BTreeSet<String>,
    /// Fixed-output derivation (a fetch: its output hash is known up front).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fixed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReducedGraph {
    pub derivations: BTreeMap<String, ReducedDrv>,
}
