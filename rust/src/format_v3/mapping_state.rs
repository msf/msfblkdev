use super::{
    checkpoint_publish::{self, LogPosition},
    index::Checkpoint,
    page_read::{PageRead, READ_AHEAD_PAGES},
    recovery::RecoveryStats,
};
use crate::{
    accounted_io::BackingIo,
    invalid_data,
    mapping_cache::{Access, Acquisition, CacheError, CachedPage, MappingCache},
};
use std::{fs::File, io};

pub(crate) struct MappingState {
    pub(crate) checkpoint: Checkpoint,
    pub(crate) cache: MappingCache,
    pub(crate) tail: LogPosition,
    pub(crate) recovery: RecoveryStats,
    pub(crate) pressure_checkpoints: u64,
    pub(crate) checkpoint_failures: u64,
    pub(crate) last_read_page: Option<u32>,
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
        let sequential = checkpoint_policy == CheckpointPolicy::Allow
            && self
                .last_read_page
                .and_then(|previous| previous.checked_add(1))
                == Some(page_id);
        self.last_read_page = (checkpoint_policy == CheckpointPolicy::Allow).then_some(page_id);
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
        let mut prefetched = None;
        let page = match acquisition {
            Acquisition::Resident(page) => page,
            Acquisition::Vacant(vacant) => {
                let limit = if sequential {
                    vacant.prefetch_limit(READ_AHEAD_PAGES)
                } else {
                    1
                };
                let mut read = PageRead::new();
                let page = vacant
                    .install(read.read(backing_io, backing, &self.checkpoint, page_id, limit)?)
                    .map_err(cache_error)?;
                prefetched = Some(read);
                page
            }
            Acquisition::CheckpointRequired(_) => {
                return Err(invalid_data(
                    "published checkpoint did not release cache capacity",
                ));
            }
        };
        let result = operation(page)?;
        if let Some(read) = prefetched {
            read.install_prefetched(&mut self.cache, &self.checkpoint, backing_io, page_id)?;
        }
        Ok(result)
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

#[cfg(test)]
pub(crate) fn read_page(
    backing_io: &mut BackingIo,
    backing: &File,
    checkpoint: &Checkpoint,
    page_id: u32,
) -> io::Result<crate::mapping_cache::MappingEntries> {
    PageRead::new().read(backing_io, backing, checkpoint, page_id, 1)
}

fn cache_error(error: CacheError) -> io::Error {
    io::Error::other(format!("mapping cache: {error:?}"))
}
