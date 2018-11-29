//! Top-level lib.rs for `cranelift_module`.

#![deny(missing_docs, trivial_numeric_casts, unused_extern_crates)]
#![warn(unused_import_braces)]
#![cfg_attr(feature = "std", deny(unstable_features))]
#![cfg_attr(
    feature = "clippy",
    plugin(clippy(conf_file = "../../clippy.toml"))
)]
#![cfg_attr(
    feature = "cargo-clippy",
    allow(new_without_default, new_without_default_derive)
)]
#![cfg_attr(
    feature = "cargo-clippy",
    warn(
        float_arithmetic,
        mut_mut,
        nonminimal_bool,
        option_map_unwrap_or,
        option_map_unwrap_or_else,
        print_stdout,
        unicode_not_nfc,
        use_self
    )
)]
// Turns on no_std and alloc features if std is not available.
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), feature(alloc))]

#[cfg(not(feature = "std"))]
#[cfg_attr(test, macro_use)]
extern crate alloc;

extern crate cranelift_codegen;
#[macro_use]
extern crate cranelift_entity;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate log;

mod backend;
mod data_context;
mod debug_context;
mod module;

pub use backend::Backend;
pub use data_context::{DataContext, DataDescription, Init};
pub use debug_context::{DebugReloc, DebugSectionContext};
pub use module::{
    DataId, DebugRelocation, DebugSectionId, FuncId, FuncOrDataId, Linkage, Module, ModuleError,
    ModuleNamespace, ModuleResult,
};

/// This replaces `std` in builds with `core`.
#[cfg(not(feature = "std"))]
mod std {
    pub use alloc::{borrow, boxed, string, vec};
    pub use core::*;
    pub mod collections {
        #[allow(unused_extern_crates)]
        extern crate hashmap_core;

        pub use self::hashmap_core::map as hash_map;
        pub use self::hashmap_core::{HashMap, HashSet};
    }
}
