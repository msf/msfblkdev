#[cfg(not(target_os = "linux"))]
compile_error!("block-storage-ublk requires Linux");

use libublk::UblkFlags;
use libublk::ctrl::UblkCtrlBuilder;
use libublk::sys::{
    UBLK_ATTR_VOLATILE_CACHE, UBLK_PARAM_TYPE_BASIC, ublk_param_basic, ublk_params,
};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

const BLOCK_SHIFT: u8 = 12;
const IO_BUFFER_BYTES: u32 = 1 << BLOCK_SHIFT;
const SECTOR_SHIFT: u8 = 9;
const SECTORS_PER_BLOCK: u64 = 1 << (BLOCK_SHIFT - SECTOR_SHIFT);
const QUEUE_COUNT: u16 = 1;
const QUEUE_DEPTH: u16 = 1;

#[derive(Debug, PartialEq, Eq)]
struct Args {
    backing_path: PathBuf,
    device_id: i32,
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> io::Result<Args> {
    let mut args = args.into_iter();
    let program = args
        .next()
        .unwrap_or_else(|| OsString::from("block-storage-ublk"));
    let usage = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "usage: {} <backing-path> <device-id>",
                PathBuf::from(&program).display()
            ),
        )
    };

    let backing_path = PathBuf::from(args.next().ok_or_else(&usage)?);
    let device_id = parse_device_id(&args.next().ok_or_else(&usage)?)?;
    if args.next().is_some() {
        return Err(usage());
    }

    Ok(Args {
        backing_path,
        device_id,
    })
}

fn parse_device_id(value: &OsStr) -> io::Result<i32> {
    let id = value
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid device ID"))?
        .parse::<i32>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid device ID"))?;
    if id < -1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "device ID must be -1 or non-negative",
        ));
    }
    Ok(id)
}

fn controller_builder(device_id: i32) -> UblkCtrlBuilder<'static> {
    UblkCtrlBuilder::default()
        .name("block-storage")
        .id(device_id)
        .nr_queues(QUEUE_COUNT)
        .depth(QUEUE_DEPTH)
        .io_buf_bytes(IO_BUFFER_BYTES)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
}

struct TargetConfiguration {
    device_bytes: u64,
    parameters: ublk_params,
}

fn target_configuration(volume_blocks: u32) -> TargetConfiguration {
    TargetConfiguration {
        device_bytes: u64::from(volume_blocks) * u64::from(IO_BUFFER_BYTES),
        parameters: ublk_params {
            types: UBLK_PARAM_TYPE_BASIC,
            basic: ublk_param_basic {
                attrs: UBLK_ATTR_VOLATILE_CACHE,
                logical_bs_shift: BLOCK_SHIFT,
                physical_bs_shift: BLOCK_SHIFT,
                io_opt_shift: BLOCK_SHIFT,
                io_min_shift: BLOCK_SHIFT,
                max_sectors: IO_BUFFER_BYTES >> SECTOR_SHIFT,
                dev_sectors: u64::from(volume_blocks) * SECTORS_PER_BLOCK,
                ..Default::default()
            },
            ..Default::default()
        },
    }
}

fn run() -> io::Result<()> {
    let args = parse_args(std::env::args_os())?;
    let volume = block_storage::open(&args.backing_path)?;
    let _controller = controller_builder(args.device_id);
    let target = target_configuration(volume.volume_blocks());
    let _device_bytes = target.device_bytes;
    let _parameters = target.parameters;

    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "ublk request handling is not implemented",
    ))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("block-storage-ublk: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backing_path_and_device_id() {
        let args = parse_args([
            OsString::from("daemon"),
            OsString::from("volume.img"),
            OsString::from("7"),
        ])
        .unwrap();

        assert_eq!(
            args,
            Args {
                backing_path: PathBuf::from("volume.img"),
                device_id: 7,
            }
        );
    }

    #[test]
    fn accepts_auto_allocated_device_id() {
        assert_eq!(parse_device_id(OsStr::new("-1")).unwrap(), -1);
    }

    #[test]
    fn rejects_invalid_arguments() {
        assert!(parse_device_id(OsStr::new("-2")).is_err());
        assert!(parse_device_id(OsStr::new("not-a-number")).is_err());
        assert!(parse_args([OsString::from("daemon")]).is_err());
        assert!(
            parse_args([
                OsString::from("daemon"),
                OsString::from("volume.img"),
                OsString::from("1"),
                OsString::from("extra"),
            ])
            .is_err()
        );
    }

    #[test]
    fn configures_exact_v07_target_parameters() {
        let target = target_configuration(37);
        let params = target.parameters;
        let basic = params.basic;

        assert_eq!(target.device_bytes, 37 * 4096);
        assert_eq!(params.len, 0);
        assert_eq!(params.types, UBLK_PARAM_TYPE_BASIC);
        assert_eq!(basic.attrs, UBLK_ATTR_VOLATILE_CACHE);
        assert_eq!(basic.logical_bs_shift, BLOCK_SHIFT);
        assert_eq!(basic.physical_bs_shift, BLOCK_SHIFT);
        assert_eq!(basic.io_opt_shift, BLOCK_SHIFT);
        assert_eq!(basic.io_min_shift, BLOCK_SHIFT);
        assert_eq!(basic.max_sectors, 8);
        assert_eq!(basic.chunk_sectors, 0);
        assert_eq!(basic.dev_sectors, 37 * 8);
        assert_eq!(basic.virt_boundary_mask, 0);
        assert_eq!(params.discard.discard_alignment, 0);
        assert_eq!(params.discard.discard_granularity, 0);
        assert_eq!(params.discard.max_discard_sectors, 0);
        assert_eq!(params.discard.max_write_zeroes_sectors, 0);
        assert_eq!(params.discard.max_discard_segments, 0);
        assert_eq!(params.discard.reserved0, 0);
        assert_eq!(params.devt.char_major, 0);
        assert_eq!(params.devt.char_minor, 0);
        assert_eq!(params.devt.disk_major, 0);
        assert_eq!(params.devt.disk_minor, 0);
        assert_eq!(params.zoned.max_open_zones, 0);
        assert_eq!(params.zoned.max_active_zones, 0);
        assert_eq!(params.zoned.max_zone_append_sectors, 0);
        assert_eq!(params.zoned.reserved, [0; 20]);
    }

    #[test]
    fn configures_one_depth_one_4k_queue() {
        assert_eq!(QUEUE_COUNT, 1);
        assert_eq!(QUEUE_DEPTH, 1);
        assert_eq!(IO_BUFFER_BYTES, 4096);

        let expected = UblkCtrlBuilder::default()
            .name("block-storage")
            .id(4)
            .nr_queues(1)
            .depth(1)
            .io_buf_bytes(4096)
            .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV);
        assert_eq!(controller_builder(4), expected);
    }
}
