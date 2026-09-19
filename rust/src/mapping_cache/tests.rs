use super::*;

fn cache(pages: u32, slots: usize) -> MappingCache {
    let bytes = u64::from(pages) * ENTRIES_PER_PAGE as u64 * crate::BLOCK_SIZE as u64;
    MappingCache::new(
        bytes,
        Some(Sizing::for_capacity(slots).unwrap().allocated_bytes as u64),
    )
    .unwrap()
}

fn install(cache: &mut MappingCache, page: u32, access: Access) -> CachedPage<'_> {
    match cache.acquire(page, access).unwrap() {
        Acquisition::Vacant(vacant) => vacant.install(MappingEntries::zeroed()).unwrap(),
        _ => panic!("expected a miss with an available slot"),
    }
}

#[test]
fn mapping_layout_and_empty_cache() {
    assert_eq!(size_of::<MappingEntries>(), 4032);
    assert_eq!(align_of::<MappingEntries>(), align_of::<u64>());
    let cache = cache(4, 2);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.resident_slots, 0);
    assert_eq!(snapshot.dirty_slots, 0);
    assert_eq!(snapshot.counters, Counters::default());
    assert_eq!(snapshot.counters.foreground.hit_rate(), None);
    assert_eq!(snapshot.counters.recovery.hit_rate(), None);
    assert_eq!(cache.dirty_pages().count(), 0);
    assert_eq!(
        snapshot.sizing.allocated_bytes,
        snapshot.sizing.slot_bytes
            + snapshot.sizing.lookup_bytes
            + snapshot.sizing.policy_bytes
            + snapshot.sizing.sort_bytes
    );
}

#[test]
fn budget_rounding_minimum_and_page_cap() {
    let logical = 100 * ENTRIES_PER_PAGE as u64 * crate::BLOCK_SIZE as u64;
    for capacity in [1, 2, 3, 7, 8, 31, 32, 63, 64, 65, 99] {
        let exact = Sizing::for_capacity(capacity).unwrap().allocated_bytes as u64;
        assert_eq!(
            Sizing::new(logical, Some(exact)).unwrap().capacity,
            capacity
        );
        let below = Sizing::new(logical, Some(exact - 1)).unwrap();
        assert_eq!(below.capacity, capacity.saturating_sub(1).max(1));
    }
    let minimum = Sizing::new(logical, Some(0)).unwrap();
    assert_eq!(minimum.capacity, 1);
    assert!(minimum.allocated_bytes > 0);
    let capped = Sizing::new(logical, Some(u64::MAX)).unwrap();
    assert_eq!(capped.capacity, 100);
    let partial = Sizing::new(337 * crate::BLOCK_SIZE as u64, Some(u64::MAX)).unwrap();
    assert_eq!(partial.page_count, 2);
    assert_eq!(partial.capacity, 2);
}

#[test]
fn rejects_invalid_geometry_capacity_and_size_overflow() {
    for bytes in [0, 1, 4095, 4097, (crate::MAX_VOLUME_BLOCKS + 1) * 4096] {
        assert!(matches!(
            MappingCache::new(bytes, None),
            Err(CacheError::InvalidGeometry)
        ));
    }
    assert_eq!(Sizing::for_capacity(0), Err(CacheError::InvalidCapacity));
    assert_eq!(
        Sizing::for_capacity(usize::MAX),
        Err(CacheError::SizeOverflow)
    );
    assert!(MappingCache::new(4096, None).is_ok());
    assert!(Sizing::new(crate::MAX_VOLUME_BLOCKS * 4096, None).is_ok());
}

#[test]
fn default_budgets_choose_largest_fitting_capacity() {
    for (logical, requested) in [
        (4_u64 << 30, 429_496),
        (400_u64 << 30, 42_949_672),
        (4_u64 << 40, 439_804_651),
    ] {
        let sizing = Sizing::new(logical, None).unwrap();
        assert_eq!(sizing.requested_bytes, requested);
        assert!(sizing.allocated_bytes as u64 <= requested);
        assert!(
            Sizing::for_capacity(sizing.capacity + 1)
                .unwrap()
                .allocated_bytes as u64
                > requested
        );
        println!(
            "logical_bytes={logical} requested={requested} slots={} allocated={} lookup={} policy={}",
            sizing.capacity, sizing.allocated_bytes, sizing.lookup_bytes, sizing.policy_bytes
        );
    }
}

#[test]
fn synthetic_zero_page_is_a_clean_miss_then_a_hit() {
    let mut cache = cache(3, 1);
    assert_eq!(
        install(&mut cache, 0, Access::Foreground).entries(),
        &MappingEntries::zeroed()
    );
    assert!(matches!(
        cache.acquire(0, Access::Foreground),
        Ok(Acquisition::Resident(_))
    ));
    assert_eq!(
        cache.snapshot().counters.foreground,
        Lookups { hits: 1, misses: 1 }
    );
    assert_eq!(cache.snapshot().counters.foreground.hit_rate(), Some(0.5));
    assert_eq!(cache.snapshot().dirty_slots, 0);
    install(&mut cache, 1, Access::Foreground);
    assert!(!cache.lookup.contains_key(&0));
    assert_eq!(cache.lookup.get(&1), Some(&0));
    assert_eq!(cache.snapshot().counters.clean_evictions, 1);
    assert_eq!(cache.snapshot().counters.victim_slots_examined, 2);
}

#[test]
fn dirty_protection_publication_and_retry_have_exact_counters() {
    let mut cache = cache(5, 2);
    install(&mut cache, 0, Access::Foreground)
        .update(0, 10, 100)
        .unwrap();
    install(&mut cache, 1, Access::Recovery);
    assert!(matches!(
        cache.acquire(0, Access::Foreground),
        Ok(Acquisition::Resident(_))
    ));
    install(&mut cache, 2, Access::Foreground)
        .update(0, 20, 200)
        .unwrap();
    assert!(cache.lookup.contains_key(&0));
    assert!(!cache.lookup.contains_key(&1));
    let Acquisition::CheckpointRequired(pending) = cache.acquire(3, Access::Recovery).unwrap()
    else {
        panic!()
    };
    // A failed publication makes no clean-state transition.
    let Acquisition::CheckpointRequired(pending) = cache.retry(pending).unwrap() else {
        panic!()
    };
    assert_eq!(
        cache
            .dirty_pages()
            .map(|(id, entries)| (id, entries.physical_addr[0]))
            .collect::<Vec<_>>(),
        vec![(0, 10), (2, 20)]
    );
    assert_eq!(
        cache.snapshot().counters,
        Counters {
            foreground: Lookups { hits: 1, misses: 2 },
            recovery: Lookups { hits: 0, misses: 2 },
            clean_evictions: 1,
            victim_slots_examined: 12,
            checkpoint_required: 2,
            ..Counters::default()
        }
    );
    cache.checkpoint_published();
    assert_eq!(cache.snapshot().dirty_slots, 0);
    let Acquisition::Vacant(vacant) = cache.retry(pending).unwrap() else {
        panic!()
    };
    vacant.install(MappingEntries::zeroed()).unwrap();
    assert!(matches!(
        cache.acquire(3, Access::Recovery),
        Ok(Acquisition::Resident(_))
    ));
    assert_eq!(
        cache.snapshot().counters,
        Counters {
            foreground: Lookups { hits: 1, misses: 2 },
            recovery: Lookups { hits: 1, misses: 2 },
            clean_evictions: 2,
            victim_slots_examined: 13,
            checkpoint_required: 2,
            ..Counters::default()
        }
    );
}

#[test]
fn invalid_updates_and_loads_preserve_contents_and_dirty_state() {
    let mut cache = MappingCache::new(337 * 4096, Some(0)).unwrap();
    assert!(matches!(
        cache.acquire(2, Access::Foreground),
        Err(CacheError::InvalidPage)
    ));
    assert_eq!(cache.snapshot().counters, Counters::default());
    {
        let mut page = install(&mut cache, 1, Access::Foreground);
        assert_eq!(page.update(1, 10, 1), Err(CacheError::InvalidEntry));
        assert_eq!(
            page.update(usize::MAX, 10, 1),
            Err(CacheError::InvalidEntry)
        );
        assert_eq!(page.update(0, 0, 1), Err(CacheError::InvalidMapping));
        assert_eq!(page.entries(), &MappingEntries::zeroed());
    }
    assert_eq!(cache.snapshot().dirty_slots, 0);
    let Acquisition::Vacant(vacant) = cache.acquire(0, Access::Foreground).unwrap() else {
        panic!()
    };
    let mut invalid = MappingEntries::zeroed();
    invalid.payload_checksum[0] = 5;
    assert!(matches!(
        vacant.install(invalid),
        Err(CacheError::InvalidMapping)
    ));
    assert!(cache.lookup.contains_key(&1));
    assert_eq!(cache.snapshot().counters.clean_evictions, 0);
    let mut other = MappingCache::new(337 * 4096, Some(0)).unwrap();
    let Acquisition::Vacant(vacant) = other.acquire(1, Access::Foreground).unwrap() else {
        panic!()
    };
    let mut invalid = MappingEntries::zeroed();
    invalid.physical_addr[1] = 10;
    assert!(matches!(
        vacant.install(invalid),
        Err(CacheError::InvalidMapping)
    ));
    assert_eq!(other.snapshot().resident_slots, 0);
    install(&mut other, 1, Access::Foreground)
        .update(0, 10, 0)
        .unwrap();
    assert_eq!(other.snapshot().dirty_slots, 1);
}

#[test]
fn cancelled_miss_does_not_evict_or_consume_a_slot() {
    let mut cache = cache(3, 1);
    assert!(matches!(
        cache.acquire(0, Access::Foreground),
        Ok(Acquisition::Vacant(_))
    ));
    assert_eq!(cache.snapshot().resident_slots, 0);
    install(&mut cache, 1, Access::Foreground);
    assert!(matches!(
        cache.acquire(2, Access::Foreground),
        Ok(Acquisition::Vacant(_))
    ));
    assert_eq!(cache.lookup.get(&1), Some(&0));
    assert_eq!(cache.snapshot().counters.clean_evictions, 0);
}

fn publish(cache: &mut MappingCache, disk: &mut [MappingEntries]) {
    for (id, entries) in cache.dirty_pages() {
        disk[id as usize] = entries.clone();
    }
    cache.checkpoint_published();
}

#[test]
fn deterministic_churn_matches_reference_mapping_without_allocation_growth() {
    let mut cache = cache(97, 7);
    let mut disk = vec![MappingEntries::zeroed(); 97];
    let mut expected = disk.clone();
    let slot_pointer = cache.slots.as_ptr();
    let initial = cache.snapshot().sizing;
    let mut seed = 0x6a09_e667_f3bc_c909_u64;
    for step in 0..20_000_u32 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let page_id = (seed % 97) as u32;
        let access = if step % 3 == 0 {
            Access::Recovery
        } else {
            Access::Foreground
        };
        let acquisition = match cache.acquire(page_id, access).unwrap() {
            Acquisition::CheckpointRequired(pending) => {
                publish(&mut cache, &mut disk);
                cache.retry(pending).unwrap()
            }
            ready => ready,
        };
        let mut page = match acquisition {
            Acquisition::Resident(page) => page,
            Acquisition::Vacant(vacant) => vacant.install(disk[page_id as usize].clone()).unwrap(),
            Acquisition::CheckpointRequired(_) => panic!("publication must release capacity"),
        };
        assert_eq!(page.entries(), &expected[page_id as usize]);
        if seed & 1 != 0 {
            let entry = (step as usize) % ENTRIES_PER_PAGE;
            page.update(entry, step + 1, u64::from(step)).unwrap();
            expected[page_id as usize].physical_addr[entry] = step + 1;
            expected[page_id as usize].payload_checksum[entry] = u64::from(step);
        }
        assert_eq!(cache.slots.as_ptr(), slot_pointer);
        assert_eq!(cache.lookup.allocation_size(), initial.lookup_bytes);
        assert_eq!(cache.clock.allocated_bytes(), initial.policy_bytes);
        assert_eq!(cache.lookup.len(), cache.resident);
        for (slot, value) in cache.slots.iter().take(cache.resident).enumerate() {
            assert_eq!(cache.lookup.get(&value.page_id), Some(&(slot as u32)));
        }
    }
    publish(&mut cache, &mut disk);
    assert_eq!(disk, expected);
    let counters = cache.snapshot().counters;
    assert_eq!(
        counters.foreground.hits + counters.foreground.misses,
        13_333
    );
    assert_eq!(counters.recovery.hits + counters.recovery.misses, 6_667);
    assert!(counters.clean_evictions > 0 && counters.checkpoint_required > 0);
}

#[test]
fn hashmap_churn_stays_preallocated_across_capacity_boundaries() {
    for capacity in [1, 2, 3, 7, 8, 31, 32, 63, 64, 65, 127] {
        let mut cache = cache(1024, capacity);
        let allocation = cache.lookup.allocation_size();
        let (_, allocations) = crate::tests::allocations::measure(|| {
            for step in 0..10_000 {
                install(&mut cache, step % 1024, Access::Foreground);
                assert_eq!(cache.lookup.allocation_size(), allocation);
            }
        });
        assert_eq!(allocations, 0);
        assert_eq!(cache.snapshot().resident_slots, capacity);
    }
}

#[test]
fn dirty_updates_pressure_retry_iteration_and_snapshots_do_not_allocate() {
    let mut cache = cache(3, 1);
    let (_, allocations) = crate::tests::allocations::measure(|| {
        install(&mut cache, 0, Access::Foreground)
            .update(0, 10, 100)
            .unwrap();
        assert!(matches!(
            cache.acquire(0, Access::Foreground),
            Ok(Acquisition::Resident(_))
        ));
        let Acquisition::CheckpointRequired(pending) = cache.acquire(1, Access::Recovery).unwrap()
        else {
            panic!()
        };
        assert_eq!(cache.dirty_pages().count(), 1);
        assert_eq!(cache.snapshot().dirty_slots, 1);
        cache.checkpoint_published();
        let Acquisition::Vacant(vacant) = cache.retry(pending).unwrap() else {
            panic!()
        };
        vacant.install(MappingEntries::zeroed()).unwrap();
    });
    assert_eq!(allocations, 0);
}

#[test]
fn sorted_dirty_pages_and_prefetch_are_bounded_and_allocation_free() {
    let mut cache = cache(20, 4);
    for id in [9, 2, 7, 1] {
        install(&mut cache, id, Access::Foreground)
            .update(0, id + 20, 0)
            .unwrap();
    }
    let before = cache.snapshot();
    let (_, allocations) = crate::tests::allocations::measure(|| {
        let ids: [u32; 4] = std::array::from_fn(|i| cache.sorted_dirty_pages().nth(i).unwrap().0);
        assert_eq!(ids, [1, 2, 7, 9]);
        assert!(!cache.prefetch(3, MappingEntries::zeroed()).unwrap());
        assert!(!cache.prefetch(9, MappingEntries::zeroed()).unwrap());
    });
    assert_eq!(allocations, 0);
    assert_eq!(cache.snapshot(), before);
    cache.checkpoint_published();
    assert!(!cache.prefetch(3, MappingEntries::zeroed()).unwrap()); // All are referenced.
    install(&mut cache, 10, Access::Foreground); // Demand ages other clean entries.
    let (_, allocations) = crate::tests::allocations::measure(|| {
        assert!(cache.prefetch(11, MappingEntries::zeroed()).unwrap());
        assert!(cache.prefetch(12, MappingEntries::zeroed()).unwrap());
        assert!(matches!(
            cache.acquire(11, Access::Foreground).unwrap(),
            Acquisition::Resident(_)
        ));
        assert!(matches!(
            cache.acquire(11, Access::Foreground).unwrap(),
            Acquisition::Resident(_)
        ));
        for id in [13, 14, 15, 16] {
            install(&mut cache, id, Access::Foreground);
        }
    });
    assert_eq!(allocations, 0);
    assert_eq!(cache.counters.prefetched_pages, 2);
    assert_eq!(cache.counters.prefetch_hits, 1);
    assert_eq!(cache.counters.prefetch_unused_evictions, 1);
}
