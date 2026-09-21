//! The resident mapping index and the dirty state of its persistent tree.
use super::{
    Geometry, RecordHeader,
    descriptor::CheckpointDescriptor,
    geometry::{DIRECT_POINTERS, DirectParent, INDEX_CHILDREN},
    index_page::{MappingIndexLevel, MappingIndexPage},
    segments::filled,
};
use crate::invalid_data;
use std::io;

pub(crate) struct MappingIndex {
    geometry: Geometry,
    /// The current snapshot block of every mapping page; zero means entirely unmapped.
    snapshot_block: Box<[u32]>,
    direct_block: Box<[u32]>,
    indirect_block: Box<[u32]>,
    direct_dirty: Box<[bool]>,
    indirect_dirty: Box<[bool]>,
}

impl MappingIndex {
    pub(crate) fn new(geometry: Geometry) -> io::Result<Self> {
        Ok(Self {
            geometry,
            snapshot_block: filled(geometry.page_count as usize, 0)?,
            direct_block: filled(geometry.direct_pages as usize, 0)?,
            indirect_block: filled(geometry.indirect_pages as usize, 0)?,
            direct_dirty: filled(geometry.direct_pages as usize, false)?,
            indirect_dirty: filled(geometry.indirect_pages as usize, false)?,
        })
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        std::mem::size_of_val(self.snapshot_block.as_ref())
            + std::mem::size_of_val(self.direct_block.as_ref())
            + std::mem::size_of_val(self.indirect_block.as_ref())
            + std::mem::size_of_val(self.direct_dirty.as_ref())
            + std::mem::size_of_val(self.indirect_dirty.as_ref())
    }

    pub(crate) fn snapshot_block(&self, page: u32) -> u32 {
        self.snapshot_block[page as usize]
    }

    pub(crate) fn snapshot_blocks(&self) -> &[u32] {
        &self.snapshot_block
    }

    /// Points `page` at a new snapshot and returns the replaced block.
    pub(crate) fn set_snapshot_block(&mut self, page: u32, block: u32) -> u32 {
        let old = std::mem::replace(&mut self.snapshot_block[page as usize], block);
        let (direct, _) = self.geometry.direct_slot(page);
        self.direct_dirty[direct as usize] = true;
        old
    }

    pub(crate) fn direct_block(&self, direct: u32) -> u32 {
        self.direct_block[direct as usize]
    }

    pub(crate) fn indirect_block(&self, indirect: u32) -> u32 {
        self.indirect_block[indirect as usize]
    }

    pub(crate) fn is_direct_dirty(&self, direct: u32) -> bool {
        self.direct_dirty[direct as usize]
    }

    pub(crate) fn is_indirect_dirty(&self, indirect: u32) -> bool {
        self.indirect_dirty[indirect as usize]
    }

    pub(crate) fn mark_direct_dirty(&mut self, direct: u32) {
        self.direct_dirty[direct as usize] = true;
    }

    pub(crate) fn mark_indirect_dirty(&mut self, indirect: u32) {
        self.indirect_dirty[indirect as usize] = true;
    }

    pub(crate) fn dirty_direct_pages(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.geometry.direct_pages).filter(|direct| self.direct_dirty[*direct as usize])
    }

    pub(crate) fn dirty_indirect_pages(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.geometry.indirect_pages).filter(|indirect| self.indirect_dirty[*indirect as usize])
    }

    pub(crate) fn dirty_page_count(&self) -> u64 {
        self.dirty_direct_pages().count() as u64 + self.dirty_indirect_pages().count() as u64
    }

    pub(crate) fn encode_direct(&self, header: RecordHeader, direct: u32) -> MappingIndexPage {
        let mut page = MappingIndexPage::empty(header, direct, MappingIndexLevel::Direct);
        let first = direct as usize * INDEX_CHILDREN;
        let children = self.geometry.direct_children(direct);
        page.child_blocks[..children]
            .copy_from_slice(&self.snapshot_block[first..first + children]);
        page
    }

    pub(crate) fn encode_indirect(&self, header: RecordHeader, indirect: u32) -> MappingIndexPage {
        let mut page = MappingIndexPage::empty(header, indirect, MappingIndexLevel::Indirect);
        let first = DIRECT_POINTERS + indirect as usize * INDEX_CHILDREN;
        let children = self.geometry.indirect_children(indirect);
        page.child_blocks[..children].copy_from_slice(&self.direct_block[first..first + children]);
        page
    }

    /// A direct page record completed at `block`; returns the replaced block.
    pub(crate) fn direct_page_written(&mut self, direct: u32, block: u32) -> u32 {
        let old = std::mem::replace(&mut self.direct_block[direct as usize], block);
        self.direct_dirty[direct as usize] = false;
        if let DirectParent::Indirect { indirect, .. } = self.geometry.direct_parent(direct) {
            self.indirect_dirty[indirect as usize] = true;
        }
        old
    }

    pub(crate) fn indirect_page_written(&mut self, indirect: u32, block: u32) -> u32 {
        self.indirect_dirty[indirect as usize] = false;
        std::mem::replace(&mut self.indirect_block[indirect as usize], block)
    }

    /// Installs a validated direct page from a root. The page starts clean.
    pub(crate) fn load_direct(
        &mut self,
        direct: u32,
        block: u32,
        page: &MappingIndexPage,
    ) -> io::Result<()> {
        if page.level != MappingIndexLevel::Direct || page.page_id != direct {
            return Err(invalid_data("direct page identity mismatch"));
        }
        let first = direct as usize * INDEX_CHILDREN;
        let children = self.geometry.direct_children(direct);
        self.snapshot_block[first..first + children]
            .copy_from_slice(&page.child_blocks[..children]);
        self.direct_block[direct as usize] = block;
        self.direct_dirty[direct as usize] = false;
        Ok(())
    }

    pub(crate) fn load_indirect(
        &mut self,
        indirect: u32,
        block: u32,
        page: &MappingIndexPage,
    ) -> io::Result<()> {
        if page.level != MappingIndexLevel::Indirect || page.page_id != indirect {
            return Err(invalid_data("indirect page identity mismatch"));
        }
        let first = DIRECT_POINTERS + indirect as usize * INDEX_CHILDREN;
        let children = self.geometry.indirect_children(indirect);
        self.direct_block[first..first + children].copy_from_slice(&page.child_blocks[..children]);
        self.indirect_block[indirect as usize] = block;
        self.indirect_dirty[indirect as usize] = false;
        Ok(())
    }

    /// Fills the descriptor's direct and indirect pointer arrays from the current tree.
    pub(crate) fn fill_descriptor(&self, descriptor: &mut CheckpointDescriptor) {
        descriptor.direct_pages.fill(0);
        descriptor.indirect_pages.fill(0);
        let direct = self.direct_block.len().min(DIRECT_POINTERS);
        descriptor.direct_pages[..direct].copy_from_slice(&self.direct_block[..direct]);
        descriptor.indirect_pages[..self.indirect_block.len()]
            .copy_from_slice(&self.indirect_block);
    }

    /// Every current record location in the tree, for rebuilding per-segment metadata counts.
    pub(crate) fn record_blocks(&self) -> impl Iterator<Item = u32> + '_ {
        self.snapshot_block
            .iter()
            .chain(self.direct_block.iter())
            .chain(self.indirect_block.iter())
            .copied()
            .filter(|block| *block != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_v4::{NO_PREVIOUS_BOUNDARY, RecordKind};

    fn header(lsn: u64) -> RecordHeader {
        RecordHeader {
            kind: RecordKind::MappingIndexPage,
            volume_id: 3,
            lsn,
            previous_boundary: NO_PREVIOUS_BOUNDARY,
            boundary: 2 + 2048,
        }
    }

    #[test]
    fn snapshot_updates_dirty_their_direct_page_and_parents() {
        // 100 GiB logical: 78 direct pages, one indirect page.
        let geometry = Geometry::new(26_214_400, 2 + 100 * 2048, 2048).unwrap();
        let mut index = MappingIndex::new(geometry).unwrap();
        assert_eq!(index.allocated_bytes(), 78_020 * 4 + 78 * 4 + 4 + 78 + 1);
        assert_eq!(index.dirty_page_count(), 0);
        assert_eq!(index.set_snapshot_block(0, 5), 0);
        assert_eq!(index.set_snapshot_block(78_019, 9), 0);
        assert_eq!(index.dirty_direct_pages().collect::<Vec<_>>(), [0, 77]);
        assert_eq!(index.dirty_indirect_pages().count(), 0);
        let page = index.encode_direct(header(1), 77);
        assert_eq!(page.child_blocks[403], 9);
        assert_eq!(page.child_blocks[404], 0);
        let mut bytes = [0; 4096];
        page.encode(geometry, &mut bytes).unwrap();
        assert_eq!(index.direct_page_written(77, 2 + 2048), 0);
        assert_eq!(index.dirty_direct_pages().collect::<Vec<_>>(), [0]);
        assert_eq!(index.dirty_indirect_pages().collect::<Vec<_>>(), [0]);
        assert_eq!(index.direct_page_written(0, 2 + 2049), 0);
        assert_eq!(index.dirty_indirect_pages().collect::<Vec<_>>(), [0]);
        let indirect = index.encode_indirect(header(2), 0);
        assert_eq!(indirect.child_blocks[77 - 64], 2 + 2048);
        indirect.encode(geometry, &mut bytes).unwrap();
        assert_eq!(index.indirect_page_written(0, 2 + 2050), 0);
        assert_eq!(index.dirty_page_count(), 0);
        let mut descriptor = super::super::descriptor::tests_support::blank(geometry);
        index.fill_descriptor(&mut descriptor);
        assert_eq!(descriptor.direct_pages[0], 2 + 2049);
        assert_eq!(descriptor.direct_pages[63], 0);
        assert_eq!(descriptor.indirect_pages[0], 2 + 2050);
        assert_eq!(index.record_blocks().count(), 5);
        assert_eq!(index.set_snapshot_block(0, 7), 5);

        // Loading rebuilds the same tree from decoded pages.
        let mut loaded = MappingIndex::new(geometry).unwrap();
        loaded.load_indirect(0, 2 + 2050, &indirect).unwrap();
        assert_eq!(loaded.direct_block(77), 2 + 2048);
        loaded.load_direct(77, 2 + 2048, &page).unwrap();
        assert_eq!(loaded.snapshot_block(78_019), 9);
        assert!(loaded.load_direct(76, 2 + 2048, &page).is_err());
        assert!(loaded.load_indirect(0, 2 + 2048, &page).is_err());
        assert_eq!(loaded.dirty_page_count(), 0);
        loaded.mark_direct_dirty(3);
        loaded.mark_indirect_dirty(0);
        assert_eq!(loaded.dirty_page_count(), 2);
    }
}
