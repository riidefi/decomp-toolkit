mod addresses;
mod relocations;
mod sections;
mod splits;
mod symbols;

use std::{
    cmp::{max, min},
    collections::{BTreeMap, BTreeSet},
    hash::Hash,
};

use anyhow::{anyhow, bail, ensure, Result};
use objdiff_core::obj::split_meta::SplitMeta;
pub use relocations::{ObjReloc, ObjRelocKind, ObjRelocations};
pub use sections::{ObjSection, ObjSectionKind, ObjSections};
pub use splits::{ObjSplit, ObjSplits};
pub use symbols::{
    best_match_for_reloc, ObjDataKind, ObjSymbol, ObjSymbolFlagSet, ObjSymbolFlags, ObjSymbolKind,
    ObjSymbolScope, ObjSymbols, SymbolIndex,
};

use crate::{
    analysis::cfa::SectionAddress,
    obj::addresses::AddressRanges,
    util::{comment::MWComment, rel::RelReloc},
};

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ObjKind {
    /// Fully linked object
    Executable,
    /// Relocatable object
    Relocatable,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ObjArchitecture {
    PowerPc,
}

/// Translation unit information.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ObjUnit {
    pub name: String,
    /// Generated, replaceable by user.
    pub autogenerated: bool,
    /// MW `.comment` section version.
    pub comment_version: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct ObjInfo {
    pub kind: ObjKind,
    pub architecture: ObjArchitecture,
    pub name: String,
    pub symbols: ObjSymbols,
    pub sections: ObjSections,
    pub entry: Option<u64>,
    pub mw_comment: Option<MWComment>,
    pub split_meta: Option<SplitMeta>,

    // Linker generated
    pub sda2_base: Option<u32>,
    pub sda_base: Option<u32>,
    pub stack_address: Option<u32>,
    pub stack_end: Option<u32>,
    pub db_stack_addr: Option<u32>,
    pub arena_lo: Option<u32>,
    pub arena_hi: Option<u32>,

    // Extracted
    pub link_order: Vec<ObjUnit>,
    pub blocked_relocation_sources: AddressRanges,
    pub blocked_relocation_targets: AddressRanges,

    // From .ctors, .dtors and extab
    pub known_functions: BTreeMap<SectionAddress, Option<u32>>,

    // REL
    /// Module ID (0 for main)
    pub module_id: u32,
    pub unresolved_relocations: Vec<RelReloc>,
}

impl ObjInfo {
    pub fn new(
        kind: ObjKind,
        architecture: ObjArchitecture,
        name: String,
        symbols: Vec<ObjSymbol>,
        sections: Vec<ObjSection>,
    ) -> Self {
        Self {
            kind,
            architecture,
            name,
            symbols: ObjSymbols::new(kind, symbols),
            sections: ObjSections::new(kind, sections),
            entry: None,
            mw_comment: Default::default(),
            split_meta: None,
            sda2_base: None,
            sda_base: None,
            stack_address: None,
            stack_end: None,
            db_stack_addr: None,
            arena_lo: None,
            arena_hi: None,
            link_order: vec![],
            blocked_relocation_sources: Default::default(),
            blocked_relocation_targets: Default::default(),
            known_functions: Default::default(),
            module_id: 0,
            unresolved_relocations: vec![],
        }
    }

    pub fn add_symbol(&mut self, in_symbol: ObjSymbol, replace: bool) -> Result<SymbolIndex> {
        match in_symbol.name.as_str() {
            "_SDA_BASE_" => self.sda_base = Some(in_symbol.address as u32),
            "_SDA2_BASE_" => self.sda2_base = Some(in_symbol.address as u32),
            "_stack_addr" => self.stack_address = Some(in_symbol.address as u32),
            "_stack_end" => self.stack_end = Some(in_symbol.address as u32),
            "_db_stack_addr" => self.db_stack_addr = Some(in_symbol.address as u32),
            "__ArenaLo" => self.arena_lo = Some(in_symbol.address as u32),
            "__ArenaHi" => self.arena_hi = Some(in_symbol.address as u32),
            _ => {}
        }
        self.symbols.add(in_symbol, replace)
    }

    pub fn add_split(&mut self, section_index: usize, address: u32, split: ObjSplit) -> Result<()> {
        let section = self
            .sections
            .get_mut(section_index)
            .ok_or_else(|| anyhow!("Invalid section index {}", section_index))?;
        let section_start = section.address as u32;
        let section_end = (section.address + section.size) as u32;
        ensure!(
            split.end == 0 || split.end <= section_end,
            "Split {} {:#010X}-{:#010X} is outside section {} {:#010X}-{:#010X}",
            split.unit,
            address,
            split.end,
            section.name,
            section_start,
            section_end
        );

        if let Some((existing_addr, existing_split)) = section.splits.for_unit(&split.unit)? {
            let new_start = min(existing_addr, address);
            let new_end = max(existing_split.end, split.end);

            // TODO use highest alignment?
            let new_align = match (split.align, existing_split.align) {
                (Some(a), Some(b)) if a == b => Some(a),
                (Some(a), Some(b)) => {
                    bail!(
                        "Conflicting alignment for split {} {} {:#010X}-{:#010X}: {:#X} != {:#X}",
                        split.unit,
                        section.name,
                        existing_addr,
                        existing_split.end,
                        a,
                        b
                    );
                }
                (Some(a), _) => Some(a),
                (_, Some(b)) => Some(b),
                _ => None,
            };

            // TODO don't merge if common flag is different?
            ensure!(
                split.common == existing_split.common,
                "Conflicting common flag for split {} {} {:#010X}-{:#010X} ({}) and {:#010X}-{:#010X} ({})",
                split.unit,
                section.name,
                existing_addr,
                existing_split.end,
                existing_split.common,
                address,
                split.end,
                split.common
            );

            // Only set autogenerated flag if both splits are autogenerated
            let new_autogenerated = split.autogenerated && existing_split.autogenerated;

            // If the new split is contained within the existing split, do nothing
            if new_start >= existing_addr && new_end <= existing_split.end {
                log::debug!(
                    "Split {} {} {:#010X}-{:#010X} already covers {:#010X}-{:#010X}",
                    split.unit,
                    section.name,
                    existing_addr,
                    existing_split.end,
                    address,
                    split.end
                );
                return Ok(());
            }

            log::debug!(
                "Extending split {} {} {:#010X}-{:#010X} to include {:#010X}-{:#010X}: {:#010X}-{:#010X}",
                split.unit,
                section.name,
                existing_addr,
                existing_split.end,
                address,
                split.end,
                new_start,
                new_end
            );

            // Check if new split overlaps any existing splits
            let mut to_remove = BTreeSet::new();
            let mut to_rename = BTreeSet::new();
            for (existing_addr, existing_split) in section.splits.for_range(new_start..new_end) {
                // TODO the logic in this method should be reworked, this is a hack
                if split.autogenerated && !existing_split.autogenerated {
                    log::debug!(
                        "-> Found existing split {} {} {:#010X}-{:#010X} (not autogenerated)",
                        existing_split.unit,
                        section.name,
                        existing_addr,
                        existing_split.end
                    );
                    return Ok(());
                }

                ensure!(
                    existing_split.autogenerated || existing_split.unit == split.unit,
                    "New split {} {} {:#010X}-{:#010X} overlaps existing split {} {:#010X}-{:#010X}",
                    split.unit,
                    section.name,
                    new_start,
                    new_end,
                    existing_split.unit,
                    existing_addr,
                    existing_split.end
                );
                log::debug!(
                    "-> Replacing existing split {} {} {:#010X}-{:#010X}",
                    existing_split.unit,
                    section.name,
                    existing_addr,
                    existing_split.end
                );
                to_remove.insert(existing_addr);
                if existing_split.unit != split.unit {
                    to_rename.insert(existing_split.unit.clone());
                }
            }

            // Remove overlapping splits
            for addr in to_remove {
                section.splits.remove(addr);
            }
            // Rename any units that were overwritten
            // TODO this should also merge with existing splits
            for unit in to_rename {
                for (existing_addr, existing) in self
                    .sections
                    .iter_mut()
                    .flat_map(|(_, section)| section.splits.iter_mut())
                    .filter(|(_, split)| split.unit == unit)
                {
                    log::debug!(
                        "-> Renaming {} {:#010X}-{:#010X} to {}",
                        existing.unit,
                        existing_addr,
                        existing.end,
                        split.unit
                    );
                    existing.unit.clone_from(&split.unit);
                }
            }
            self.add_split(section_index, new_start, ObjSplit {
                unit: split.unit,
                end: new_end,
                align: new_align,
                common: split.common,
                autogenerated: new_autogenerated,
                skip: false,  // ?
                rename: None, // ?
            })?;
            return Ok(());
        }

        log::debug!("Adding split @ {} {:#010X}: {:?}", section.name, address, split);
        section.splits.push(address, split);
        Ok(())
    }

    pub fn is_unit_autogenerated(&self, unit: &str) -> bool {
        self.sections
            .all_splits()
            .filter(|(_, _, _, split)| split.unit == unit)
            .all(|(_, _, _, split)| split.autogenerated)
    }

    /// Calculate the total size of all code sections.
    pub fn code_size(&self) -> u32 {
        self.sections
            .iter()
            .filter(|(_, section)| section.kind == ObjSectionKind::Code)
            .map(|(_, section)| section.size as u32)
            .sum()
    }

    /// Calculate the total size of all data sections, including common BSS symbols.
    pub fn data_size(&self) -> u32 {
        self.sections
            .iter()
            .filter(|(_, section)| section.kind != ObjSectionKind::Code)
            .map(|(_, section)| section.size as u32)
            .chain(
                // Include common symbols
                self.symbols
                    .iter()
                    .filter(|&symbol| symbol.flags.is_common())
                    .map(|s| s.size as u32),
            )
            .sum()
    }
}
