pub(super) use crate::config::{SqliteCase as Case, SqliteProfile as Profile};
use serde_json::{Value, json};

pub(super) const PROFILE_ENV: &str = "BLOCK_STORAGE_SQLITE_PROFILE";
pub(super) const CACHE_PAGES: u64 = 256;
pub(super) const PAGE_BYTES: u64 = 4096;
pub(super) const MMAP_BYTES: u64 = 0;
pub(super) const SEED: &str = "0x6a09e667f3bcc909";
const READ_OPERATIONS: u64 = 4096;
const UPDATE_OPERATIONS: u64 = 256;
const SHORT_BLOBS: u64 = 32;
const LONG_OPERATIONS: u64 = 4096;
const LONG_BLOBS: u64 = 2;
const SMALL_BLOB_BYTES: u64 = 128;
const LARGE_BLOB_BYTES: u64 = 8192;

// SQLite 3.51.3 speedtest1 testset "main" emits these numbered result rows.
pub(super) const SPEEDTEST_MAIN_IDS: [u64; 32] = [
    100, 110, 120, 130, 140, 142, 145, 150, 160, 161, 170, 180, 190, 200, 210, 230, 240, 250, 260,
    270, 280, 290, 300, 310, 320, 400, 410, 500, 510, 520, 980, 990,
];

impl Profile {
    pub(super) const fn journal(self) -> &'static str {
        match self {
            Self::DeleteExtra => "delete",
            Self::WalFull | Self::WalNormal => "wal",
        }
    }

    pub(super) const fn synchronous(self) -> u64 {
        match self {
            Self::DeleteExtra => 3,
            Self::WalFull => 2,
            Self::WalNormal => 1,
        }
    }

    pub(super) const fn is_wal(self) -> bool {
        matches!(self, Self::WalFull | Self::WalNormal)
    }
}

impl Case {
    pub(super) const fn is_speedtest(self) -> bool {
        matches!(self, Self::SpeedtestMain)
    }

    pub(super) const fn updates(self) -> bool {
        matches!(
            self,
            Self::ProbeSeededRandomUpdate
                | Self::KvSequentialUpdateSmallSingleTransaction
                | Self::KvSequentialUpdateSmallPerOpCommit
                | Self::KvRandomUpdateLargeSingleTransaction
                | Self::KvRandomUpdateLargePerOpCommit
                | Self::KvLongWalCheckpointStress
        )
    }

    pub(super) const fn random(self) -> bool {
        matches!(
            self,
            Self::ProbeSeededRandomUpdate
                | Self::KvRandomReadSmall
                | Self::KvRandomReadLarge
                | Self::KvRandomUpdateLargeSingleTransaction
                | Self::KvRandomUpdateLargePerOpCommit
                | Self::KvLongWalCheckpointStress
        )
    }

    pub(super) const fn blob_bytes(self) -> u64 {
        match self {
            Self::KvSequentialReadSmall
            | Self::KvRandomReadSmall
            | Self::KvSequentialUpdateSmallSingleTransaction
            | Self::KvSequentialUpdateSmallPerOpCommit => SMALL_BLOB_BYTES,
            _ => LARGE_BLOB_BYTES,
        }
    }

    pub(super) const fn operations(self) -> u64 {
        match self {
            Self::ProbeSeededRandomUpdate | Self::KvLongWalCheckpointStress => LONG_OPERATIONS,
            Self::KvSequentialReadSmall
            | Self::KvRandomReadSmall
            | Self::KvSequentialReadLarge
            | Self::KvRandomReadLarge => READ_OPERATIONS,
            Self::SpeedtestMain => 0,
            _ => UPDATE_OPERATIONS,
        }
    }

    pub(super) const fn dataset_blobs(self) -> u64 {
        if matches!(
            self,
            Self::ProbeSeededRandomUpdate | Self::KvLongWalCheckpointStress
        ) {
            LONG_BLOBS
        } else {
            SHORT_BLOBS
        }
    }

    pub(super) const fn multitrans(self) -> bool {
        matches!(
            self,
            Self::ProbeSeededRandomUpdate
                | Self::KvSequentialUpdateSmallPerOpCommit
                | Self::KvRandomUpdateLargePerOpCommit
                | Self::KvLongWalCheckpointStress
        )
    }

    pub(super) const fn benchmark_operations(self) -> Option<u64> {
        if self.is_speedtest() {
            None
        } else {
            Some(self.operations())
        }
    }

    pub(super) const fn snapshot_connection(self) -> u8 {
        if self.is_speedtest() { 1 } else { 2 }
    }

    pub(super) fn parameters(self) -> Value {
        if self.is_speedtest() {
            return json!({
                "tool": "speedtest1", "testset": "main", "size": 1,
                "page_bytes": PAGE_BYTES, "cache_pages": CACHE_PAGES,
                "mmap_bytes": MMAP_BYTES, "concurrency": 1,
                "built_in_verification": true,
            });
        }
        json!({
            "tool": "kvtest", "operations": self.operations(),
            "dataset_blobs": self.dataset_blobs(), "blob_bytes": self.blob_bytes(),
            "page_bytes": PAGE_BYTES,
            "transaction_size": if self.multitrans() { 1 } else { self.operations() },
            "concurrency": 1,
            "kvtest_key_order": if self.random() { "upstream-random" } else { "upstream-ascending" },
            "kvtest_key_order_seeded_by_adapter": false,
            "adapter_value_generator": {"identity": "splitmix64-byte", "version": 1, "seed": SEED},
            "operation": if self.updates() { "update" } else { "read" },
            "multitrans": self.multitrans(), "blob_api": false,
            "cache_pages": CACHE_PAGES, "mmap_bytes": MMAP_BYTES,
            "wal_autocheckpoint_frames": 1000,
        })
    }
}
