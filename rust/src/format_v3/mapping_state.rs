use super::{
    checkpoint_publish::{self, LogPosition},
    index::Checkpoint,
    mapping_page::{MappingPageRecord, PageExpectation},
    recovery::RecoveryStats,
};
use crate::{
    AlignedBlock, BLOCK_SIZE,
    accounted_io::BackingIo,
    invalid_data,
    mapping_cache::{Access, Acquisition, CacheError, CachedPage, MappingCache, MappingEntries},
    read_block_exact,
};
use std::{fs::File, io};

pub(crate) struct MappingState {
    pub(crate) checkpoint: Checkpoint,
    pub(crate) cache: MappingCache,
    pub(crate) tail: LogPosition,
    pub(crate) recovery: RecoveryStats,
    pub(crate) pressure_checkpoints: u64,
    pub(crate) checkpoint_failures: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointPolicy {
    Allow,
    Forbid,
}

impl MappingState {
    pub(crate) fn with_page<T>(
        &mut self,
        backing_io: &mut BackingIo,
        backing: &File,
        page_id: u32,
        checkpoint_policy: CheckpointPolicy,
        operation: impl FnOnce(CachedPage<'_>) -> io::Result<T>,
    ) -> io::Result<T> {
        let acquisition = match self
            .cache
            .acquire(page_id, Access::Foreground)
            .map_err(cache_error)?
        {
            Acquisition::CheckpointRequired(pending) => {
                assert!(
                    checkpoint_policy == CheckpointPolicy::Allow,
                    "write-buffer preflight must reserve all mapping slots"
                );
                match checkpoint_publish::publish(
                    backing_io,
                    backing,
                    &mut self.checkpoint,
                    &mut self.cache,
                    self.tail,
                ) {
                    Ok(tail) => {
                        self.tail = tail;
                        self.pressure_checkpoints += 1;
                    }
                    Err(error) => {
                        self.checkpoint_failures += 1;
                        return Err(error);
                    }
                }
                self.cache.retry(pending).map_err(cache_error)?
            }
            ready => ready,
        };
        let page = match acquisition {
            Acquisition::Resident(page) => page,
            Acquisition::Vacant(vacant) => vacant
                .install(read_page(backing_io, backing, &self.checkpoint, page_id)?)
                .map_err(cache_error)?,
            Acquisition::CheckpointRequired(_) => {
                return Err(invalid_data(
                    "published checkpoint did not release cache capacity",
                ));
            }
        };
        operation(page)
    }

    pub(crate) fn publish(&mut self, backing_io: &mut BackingIo, backing: &File) -> io::Result<()> {
        match checkpoint_publish::publish(
            backing_io,
            backing,
            &mut self.checkpoint,
            &mut self.cache,
            self.tail,
        ) {
            Ok(tail) => {
                self.tail = tail;
                Ok(())
            }
            Err(error) => {
                self.checkpoint_failures += 1;
                Err(error)
            }
        }
    }
}

pub(crate) fn read_page(
    backing_io: &mut BackingIo,
    backing: &File,
    checkpoint: &Checkpoint,
    page_id: u32,
) -> io::Result<MappingEntries> {
    let descriptor = checkpoint.descriptor;
    let geometry = descriptor.geometry()?;
    geometry.page_entries(page_id)?;
    let boundary = checkpoint.mapping_page_block[page_id as usize];
    if boundary == 0 {
        return Ok(MappingEntries::zeroed());
    }
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    read_block_exact(backing_io, backing, boundary, &mut bytes)?;
    backing_io.mapping_io.page_reads += 1;
    Ok(MappingPageRecord::decode(
        &bytes.0,
        &PageExpectation {
            geometry,
            volume_id: descriptor.volume_id,
            page_id,
            boundary,
            maximum_lsn: descriptor.checkpoint_lsn,
        },
    )?
    .entries)
}

fn cache_error(error: CacheError) -> io::Error {
    io::Error::other(format!("mapping cache: {error:?}"))
}
