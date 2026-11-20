//! Wire format v1: one scheduled fault per epoch. Each write is followed by FLUSH.
use crate::accounted_io::fault_plan::Action;
use io_uring::opcode;
use std::{fmt, io};

pub(super) const MAX_EPOCHS: usize = 4;
const MAX_WRITES: usize = 6;
// Keep the entire schedule in one data segment, with room for the baseline record.
// LBAs 6 and 7 are reserved for the independent misdirection control.
const DATA_LBAS: u64 = 6;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    RejectWrite,
    ShortWrite,
    CompletedWrite,
    RejectSync,
    CompletedSync,
}
use Effect::*;

const EFFECTS: [(Effect, &str); 5] = [
    (RejectWrite, "write-enospc"),
    (ShortWrite, "write-short"),
    (CompletedWrite, "write-after-eio"),
    (RejectSync, "sync-eio"),
    (CompletedSync, "sync-after-eio"),
];

pub(super) struct Epoch {
    pub effect: Effect,
    pub ordinal: usize,
    pub writes: Vec<(u32, u8)>,
}

impl Effect {
    pub fn fault(self) -> (u8, Action) {
        match self {
            RejectWrite => (opcode::Writev::CODE, Action::Reject(libc::ENOSPC)),
            ShortWrite => (opcode::Writev::CODE, Action::ShortZero),
            CompletedWrite => (opcode::Writev::CODE, Action::ErrorAfterSuccess(libc::EIO)),
            RejectSync => (opcode::Fsync::CODE, Action::Reject(libc::EIO)),
            CompletedSync => (opcode::Fsync::CODE, Action::ErrorAfterSuccess(libc::EIO)),
        }
    }
}

pub(super) struct Plan(pub Vec<Epoch>);

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "format4-dst-v1")?;
        for epoch in &self.0 {
            write!(
                f,
                "{} {}",
                EFFECTS
                    .iter()
                    .find(|(effect, _)| *effect == epoch.effect)
                    .expect("all v1 effects named")
                    .1,
                epoch.ordinal
            )?;
            for (lba, value) in &epoch.writes {
                write!(f, " {lba}:{value}")?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

fn invalid() -> io::Error {
    io::Error::other("invalid format4-dst-v1 plan")
}

pub(super) fn parse(text: &str) -> io::Result<Plan> {
    let mut lines = text.lines();
    if lines.next() != Some("format4-dst-v1") {
        return Err(invalid());
    }
    let mut epochs = Vec::new();
    for line in lines {
        if epochs.len() == MAX_EPOCHS {
            return Err(invalid());
        }
        let mut fields = line.split_ascii_whitespace();
        let name = fields.next().ok_or_else(invalid)?;
        let effect = EFFECTS
            .iter()
            .find_map(|&(effect, n)| (n == name).then_some(effect))
            .ok_or_else(invalid)?;
        let ordinal = fields
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| (1..=MAX_WRITES + 1).contains(n))
            .ok_or_else(invalid)?;
        let mut writes = Vec::new();
        for field in fields {
            let (l, v) = field.split_once(':').ok_or_else(invalid)?;
            let lba = l
                .parse::<u32>()
                .ok()
                .filter(|&n| u64::from(n) < DATA_LBAS)
                .ok_or_else(invalid)?;
            let value = v
                .parse::<u8>()
                .ok()
                .filter(|n| *n != 0)
                .ok_or_else(invalid)?;
            if writes.len() == MAX_WRITES {
                return Err(invalid());
            }
            writes.push((lba, value));
        }
        if writes.is_empty() {
            return Err(invalid());
        }
        epochs.push(Epoch {
            effect,
            ordinal,
            writes,
        });
    }
    if epochs.is_empty() {
        return Err(invalid());
    }
    Ok(Plan(epochs))
}

pub(super) fn generate(mut seed: u64) -> Plan {
    // SplitMix64 reference constants (Steele, Lea and Flood). v1 also fixes draw order.
    let mut draw = |limit: u64| {
        seed = seed.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        (z ^ (z >> 31)) % limit
    };
    let mut epochs = Vec::new();
    for _ in 0..MAX_EPOCHS {
        let effect = EFFECTS[draw(EFFECTS.len() as u64) as usize].0;
        let ordinal = 1 + draw(MAX_WRITES as u64) as usize;
        let writes = (0..ordinal)
            .map(|_| (draw(DATA_LBAS) as u32, 1 + draw(255) as u8))
            .collect();
        epochs.push(Epoch {
            effect,
            ordinal,
            writes,
        });
    }
    Plan(epochs)
}

#[test]
fn v1_wire_format_and_bounds() {
    let golden = "format4-dst-v1\nsync-eio 2 0:40 4:238\nwrite-enospc 3 1:210 5:182 2:11\nwrite-short 3 5:142 3:169 0:152\nwrite-enospc 2 2:226 1:122\n";
    assert_eq!(generate(42).to_string(), golden);
    assert_eq!(parse(golden).unwrap().to_string(), golden);
    for line in [
        "unknown 1 0:1",
        "sync-eio 0 0:1",
        "sync-eio 1 6:1",
        "sync-eio 1 1:256",
        "sync-eio 1",
        "sync-eio 1 1:1 2:2 3:3 4:4 5:5 0:6 1:7",
    ] {
        assert!(parse(&format!("format4-dst-v1\n{line}")).is_err(), "{line}");
    }
    assert!(parse("format4-dst-v2").is_err());
    assert!(parse(&format!("{golden}sync-eio 1 0:1")).is_err());
}
