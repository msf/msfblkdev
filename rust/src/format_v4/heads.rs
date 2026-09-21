//! Append cursors for the two fixed heads and the segment-aware packing plan.
use super::{Geometry, HeadId, NO_PREVIOUS_BOUNDARY, RecordHeader};
use crate::MAXIMUM_PAYLOAD_BLOCKS;

/// One append position: the last record's LSN and boundary.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LogPosition {
    pub(crate) lsn: u64,
    pub(crate) boundary: u32,
}

/// The writer's cursor for one head. Every segment keeps its final block for the closing link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) id: HeadId,
    /// The open segment; comes from the segment table, never from the boundary.
    pub(crate) segment: u32,
    /// The next block to write.
    pub(crate) next_block: u32,
    /// The last record boundary on this chain; `NO_PREVIOUS_BOUNDARY` before the first record.
    pub(crate) last_boundary: u32,
    pub(crate) last_lsn: u64,
}

impl Head {
    pub(crate) fn opened(id: HeadId, segment: u32, geometry: Geometry) -> std::io::Result<Self> {
        Ok(Self {
            id,
            segment,
            next_block: geometry.segment_start(segment)?,
            last_boundary: NO_PREVIOUS_BOUNDARY,
            last_lsn: 0,
        })
    }

    /// Blocks still writable before the reserved link block.
    pub(crate) fn room(self, geometry: Geometry) -> u32 {
        let end = geometry
            .segment_end(self.segment)
            .expect("open segment exists");
        u32::try_from(
            end.saturating_sub(1)
                .saturating_sub(u64::from(self.next_block)),
        )
        .expect("segment room fits u32")
    }

    /// Payloads a data record appended now could hold, or zero when the segment must close.
    pub(crate) fn payload_room(self, geometry: Geometry) -> usize {
        let room = self.room(geometry);
        if room < 2 {
            return 0;
        }
        (room as usize - 1).min(MAXIMUM_PAYLOAD_BLOCKS)
    }

    /// The block where the closing link goes: the next block, which the reserve guarantees.
    pub(crate) fn link_block(self, geometry: Geometry) -> u32 {
        let end = geometry
            .segment_end(self.segment)
            .expect("open segment exists");
        assert!(u64::from(self.next_block) < end, "link slot is reserved");
        self.next_block
    }

    pub(crate) fn header(
        self,
        kind: super::RecordKind,
        volume_id: u64,
        lsn: u64,
        payloads: u32,
    ) -> RecordHeader {
        RecordHeader {
            kind,
            volume_id,
            lsn,
            previous_boundary: self.last_boundary,
            boundary: self.next_block + payloads,
        }
    }

    /// A record with `header` completed on this head.
    pub(crate) fn advance(&mut self, header: RecordHeader) {
        debug_assert_eq!(header.previous_boundary, self.last_boundary);
        debug_assert!(header.lsn > self.last_lsn);
        self.last_boundary = header.boundary;
        self.last_lsn = header.lsn;
        self.next_block = header.boundary + 1;
    }

    /// The head moves to `segment` after its link completed; the link is the last record so far.
    pub(crate) fn open(&mut self, segment: u32, geometry: Geometry) -> std::io::Result<()> {
        self.segment = segment;
        self.next_block = geometry.segment_start(segment)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn position(self) -> LogPosition {
        LogPosition {
            lsn: self.last_lsn,
            boundary: self.last_boundary,
        }
    }
}

/// What appending a workload would consume, simulated against the packing rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AppendPlan {
    /// Payload, footer, metadata and link blocks written.
    pub(crate) blocks: u64,
    pub(crate) records: u64,
    pub(crate) links: u32,
    /// Free segments the plan opens.
    pub(crate) new_segments: u32,
}

/// Plans `payloads` raw payload blocks followed by `metadata` one-block records on a head with
/// `room` writable blocks left. Records never cross a segment; each segment keeps a link slot.
pub(crate) fn plan_append(
    geometry: Geometry,
    room: u32,
    payloads: u64,
    metadata: u64,
) -> AppendPlan {
    let fresh = geometry.blocks_per_segment - 1;
    let mut plan = AppendPlan::default();
    let mut room = u64::from(room);
    let mut remaining = payloads;
    // Bounded: every iteration either consumes payloads or opens a segment with room for them.
    while remaining > 0 {
        if room >= 2 {
            let take = remaining.min(MAXIMUM_PAYLOAD_BLOCKS as u64).min(room - 1);
            room -= take + 1;
            remaining -= take;
            plan.blocks += take + 1;
            plan.records += 1;
        } else {
            plan.blocks += 1;
            plan.links += 1;
            plan.new_segments += 1;
            room = u64::from(fresh);
        }
    }
    let mut remaining = metadata;
    while remaining > 0 {
        if room >= 1 {
            let take = remaining.min(room);
            room -= take;
            remaining -= take;
            plan.blocks += take;
            plan.records += take;
        } else {
            plan.blocks += 1;
            plan.links += 1;
            plan.new_segments += 1;
            room = u64::from(fresh);
        }
    }
    plan
}

/// Free segments that user writes may never consume: one worst-case victim relocation plus two
/// complete checkpoint publications, each starting on a head whose segment is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reserve {
    pub(crate) relocation_segments: u32,
    pub(crate) publication_segments: u32,
    pub(crate) segments: u32,
}

impl Reserve {
    pub(crate) fn compute(geometry: Geometry, cache_capacity: u64) -> Self {
        let victim_payloads = u64::from(geometry.blocks_per_segment - 2);
        let relocation_segments = plan_append(geometry, 0, victim_payloads, 0).new_segments;
        let publication_blocks = cache_capacity
            + u64::from(geometry.direct_pages)
            + u64::from(geometry.indirect_pages)
            + u64::from(geometry.table_pages);
        let publication_segments = plan_append(geometry, 0, 0, publication_blocks).new_segments;
        Self {
            relocation_segments,
            publication_segments,
            segments: relocation_segments + 2 * publication_segments,
        }
    }
}

/// Blocks foreground writes may still claim: the User head's room plus the free segments above
/// the reserve, each minus its link slot.
pub(crate) fn user_available_blocks(
    geometry: Geometry,
    user_room: u32,
    allocatable_segments: u32,
    reserve: Reserve,
) -> u64 {
    u64::from(user_room)
        + u64::from(allocatable_segments.saturating_sub(reserve.segments))
            * u64::from(geometry.blocks_per_segment - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_v4::RecordKind;

    fn geometry() -> Geometry {
        Geometry::new(1000, 2 + 6 * 8, 8).unwrap()
    }

    #[test]
    fn head_cursor_reserves_the_link_slot_and_restarts_after_a_link() {
        let geometry = geometry();
        let mut head = Head::opened(HeadId::User, 2, geometry).unwrap();
        assert_eq!(head.next_block, 2 + 16);
        assert_eq!(head.room(geometry), 7);
        assert_eq!(head.payload_room(geometry), 6);
        let header = head.header(RecordKind::Data, 5, 1, 4);
        assert_eq!(
            header,
            RecordHeader {
                kind: RecordKind::Data,
                volume_id: 5,
                lsn: 1,
                previous_boundary: NO_PREVIOUS_BOUNDARY,
                boundary: 2 + 16 + 4,
            }
        );
        head.advance(header);
        assert_eq!(
            head.position(),
            LogPosition {
                lsn: 1,
                boundary: 22
            }
        );
        assert_eq!(head.room(geometry), 2);
        assert_eq!(head.payload_room(geometry), 1);
        let one = head.header(RecordKind::Data, 5, 3, 1);
        head.advance(one);
        assert_eq!(head.room(geometry), 0);
        assert_eq!(head.payload_room(geometry), 0);
        assert_eq!(head.link_block(geometry), 2 + 16 + 7);
        let link = head.header(RecordKind::SegmentLink, 5, 4, 0);
        assert_eq!(link.boundary, 25);
        head.advance(link);
        head.open(4, geometry).unwrap();
        assert_eq!(head.next_block, 2 + 32);
        assert_eq!(head.last_boundary, 25);
        assert_eq!(head.room(geometry), 7);
        let big = Geometry::new(1000, 2 + 4 * 1024, 1024).unwrap();
        let fresh = Head::opened(HeadId::Relocation, 1, big).unwrap();
        assert_eq!(fresh.payload_room(big), 338);
    }

    #[test]
    fn append_plan_matches_hand_counted_packing() {
        let geometry = geometry();
        assert_eq!(
            plan_append(geometry, 7, 6, 0),
            AppendPlan {
                blocks: 7,
                records: 1,
                links: 0,
                new_segments: 0
            }
        );
        // Seven payloads: six fit, then a link, then one payload and its footer.
        assert_eq!(
            plan_append(geometry, 7, 7, 0),
            AppendPlan {
                blocks: 7 + 1 + 2,
                records: 2,
                links: 1,
                new_segments: 1
            }
        );
        // No room at all: link first.
        assert_eq!(
            plan_append(geometry, 0, 1, 1),
            AppendPlan {
                blocks: 1 + 2 + 1,
                records: 2,
                links: 1,
                new_segments: 1
            }
        );
        // One block left cannot hold a data record but holds one metadata record.
        assert_eq!(
            plan_append(geometry, 1, 0, 3),
            AppendPlan {
                blocks: 1 + 1 + 2,
                records: 3,
                links: 1,
                new_segments: 1
            }
        );
        assert_eq!(plan_append(geometry, 0, 0, 0), AppendPlan::default());
        let big = Geometry::new(1000, 2 + 4 * 1024, 1024).unwrap();
        let plan = plan_append(big, 1023, 1500, 0);
        // 338 + 338 + 338 = 1014 payloads plus three footers fill 1017 blocks, leaving 6: five
        // more payloads and a footer fit exactly, then a link and the last 481 payloads.
        assert_eq!(plan.records, 4 + 2);
        assert_eq!(plan.links, 1);
        assert_eq!(plan.blocks, 1500 + 6 + 1);
    }

    #[test]
    fn reserve_and_user_availability_follow_the_plan() {
        let geometry = geometry();
        let reserve = Reserve::compute(geometry, 4);
        // Six live payloads plus a footer need one fresh segment; 4 + 1 + 1 metadata blocks too.
        assert_eq!(reserve.relocation_segments, 1);
        assert_eq!(reserve.publication_segments, 1);
        assert_eq!(reserve.segments, 3);
        assert_eq!(user_available_blocks(geometry, 5, 4, reserve), 5 + 7);
        assert_eq!(user_available_blocks(geometry, 5, 3, reserve), 5);
        assert_eq!(user_available_blocks(geometry, 0, 0, reserve), 0);
        let big = Geometry::new(1 << 20, 2 + 64 * 2048, 2048).unwrap();
        let reserve = Reserve::compute(big, 4096);
        // 2046 payloads and seven footers need 2053 blocks, so two fresh segments of 2047.
        assert_eq!(reserve.relocation_segments, 2);
        // 4096 snapshots + 4 direct pages + 1 table page = 4101 blocks: three segments.
        assert_eq!(reserve.publication_segments, 3);
        assert_eq!(reserve.segments, 8);
    }
}
