//! Defines `DebugSectionContext`.

use cranelift_codegen::binemit::{Addend, CodeOffset};
use cranelift_codegen::ir;
use std::vec::Vec;

/// A relocation in a debug section.
pub struct DebugReloc {
    /// The offset within the debug section of the relocation.
    pub offset: CodeOffset,
    /// The size in bytes of the relocation.
    pub size: u8,
    /// The symbol that the relocation is a reference to.
    pub name: ir::ExternalName,
    /// The addend to add to the symbol value.
    pub addend: Addend,
}

/// The information used to define a debug section.
pub struct DebugSectionContext {
    /// The section data.
    pub data: Vec<u8>,
    /// Addresses to write at specified offsets.
    pub relocs: Vec<DebugReloc>,
}

impl DebugSectionContext {
    /// Allocate a new context.
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            relocs: Vec::new(),
        }
    }
}
