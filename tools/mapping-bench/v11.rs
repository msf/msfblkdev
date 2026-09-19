fn options(_budget: Option<u64>) -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: 4 << 20,
        checkpoint_after_bytes: 64 << 20,
    }
}

fn stats(volume: &Volume) -> CacheStats {
    CacheStats {
        index_bytes: volume.volume_blocks() as usize * 12,
        ..CacheStats::default()
    }
}
