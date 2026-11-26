//! `blkctl`: inspect and check a format-4 backing (ADR-11).
//!
//! `info` reads one checkpoint and is safe on a backing that a daemon serves. `check` verifies
//! every checksum and reference and refuses a backing that a live volume owns.
use block_storage::{
    BackingKind, CheckpointSlot, Finding, HeadId, LogTotals, RootStatus, SegmentState, SegmentView,
    Severity, TailSummary, VolumeView,
};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
usage: blkctl info  [--segments] [--blocks] [-v] [--color=auto|always|never] <backing>
       blkctl check [-v] [--color=auto|always|never] <backing>";
const BLOCK_BYTES: u64 = 4096;
/// Findings printed without `-v`; the total is always printed.
const MAX_ERRORS_SHOWN: u64 = 20;
/// Characters of the longest live-fraction bar.
const HISTOGRAM_WIDTH: u64 = 40;

/// `info`: every descriptor and page it read is valid. `check`: no errors and no tail.
const EXIT_CLEAN: u8 = 0;
/// `check` only: no errors, and a tail exists that the next `open_live` replays.
const EXIT_TAIL: u8 = 1;
/// A descriptor or page failed validation (`info`), or errors were found (`check`).
const EXIT_DAMAGED: u8 = 4;
/// Could not run: usage, I/O, not format 4, owned by a live volume, or changed too fast.
const EXIT_NOT_RUN: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    Help,
    Info {
        backing: PathBuf,
        segments: bool,
        blocks: bool,
        verbose: bool,
        color: ColorChoice,
    },
    Check {
        backing: PathBuf,
        verbose: bool,
        color: ColorChoice,
    },
}

fn main() -> ExitCode {
    let command = match parse_args(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("blkctl: {message}\n{USAGE}");
            return ExitCode::from(EXIT_NOT_RUN);
        }
    };
    let code = match command {
        Command::Help => {
            println!("{USAGE}");
            EXIT_CLEAN
        }
        Command::Info {
            backing,
            segments,
            blocks,
            verbose,
            color,
        } => {
            let options = InfoOptions {
                segments,
                blocks,
                verbose,
                style: Style::new(color),
            };
            run_info(&backing, &options)
        }
        Command::Check {
            backing,
            verbose,
            color,
        } => run_check(&backing, verbose, Style::new(color)),
    };
    ExitCode::from(code)
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let subcommand = args.next().ok_or("missing command")?;
    let subcommand = subcommand
        .to_str()
        .ok_or("command is not UTF-8")?
        .to_owned();
    if matches!(subcommand.as_str(), "-h" | "--help" | "help") {
        return Ok(Command::Help);
    }
    let is_info = match subcommand.as_str() {
        "info" => true,
        "check" => false,
        other => return Err(format!("unknown command {other:?}")),
    };
    let (mut segments, mut blocks, mut verbose) = (false, false, false);
    let mut color = ColorChoice::Auto;
    let mut backing = None;
    for arg in args {
        match arg.to_str() {
            Some("-h" | "--help") => return Ok(Command::Help),
            Some("--segments") if is_info => segments = true,
            Some("--blocks") if is_info => blocks = true,
            Some("-v" | "--verbose") => verbose = true,
            Some(option) if option.starts_with("--color=") => color = parse_color(option)?,
            Some(option) if option.starts_with('-') && option != "-" => {
                return Err(format!("unknown option {option:?} for {subcommand}"));
            }
            _ if backing.is_some() => return Err("expected one backing path".to_owned()),
            _ => backing = Some(PathBuf::from(arg)),
        }
    }
    let backing = backing.ok_or("missing backing path")?;
    Ok(if is_info {
        Command::Info {
            backing,
            segments,
            blocks,
            verbose,
            color,
        }
    } else {
        Command::Check {
            backing,
            verbose,
            color,
        }
    })
}

fn parse_color(option: &str) -> Result<ColorChoice, String> {
    match option.trim_start_matches("--color=") {
        "auto" => Ok(ColorChoice::Auto),
        "always" => Ok(ColorChoice::Always),
        "never" => Ok(ColorChoice::Never),
        other => Err(format!("unknown color choice {other:?}")),
    }
}

/// ANSI styling, or none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Style {
    enabled: bool,
}

impl Style {
    fn new(choice: ColorChoice) -> Self {
        let enabled = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                io::stdout().is_terminal()
                    && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
                    && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
            }
        };
        Self { enabled }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }

    fn finding(self, finding: &Finding) -> String {
        let line = finding.to_string();
        let (severity, rest) = line.split_at(line.find(' ').unwrap_or(line.len()));
        let severity = match finding.severity {
            Severity::Error => self.red(severity),
            Severity::Warning => self.yellow(severity),
        };
        format!("{severity}{rest}")
    }
}

fn emit(text: &str) {
    // A closed stdout (for example `| head`) is not an error worth reporting.
    let _ = io::stdout().lock().write_all(text.as_bytes());
}

#[derive(Clone, Copy, Debug)]
struct InfoOptions {
    segments: bool,
    blocks: bool,
    verbose: bool,
    style: Style,
}

fn run_info(backing: &Path, options: &InfoOptions) -> u8 {
    let view = match block_storage::open_readonly(backing) {
        Ok(view) => view,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            let line = format!("error  {}: {error}", backing.display());
            emit(&format!("{}\n1 error\n", options.style.red(&line)));
            return EXIT_DAMAGED;
        }
        Err(error) => {
            eprintln!("blkctl: cannot read {}: {error}", backing.display());
            return EXIT_NOT_RUN;
        }
    };
    emit(&render_info(&view, backing, options));
    let damaged = view
        .roots
        .iter()
        .any(|root| matches!(root, RootStatus::Invalid { .. }));
    if damaged { EXIT_DAMAGED } else { EXIT_CLEAN }
}

/// How sizes print: binary units with one decimal, or 4 KiB block counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Units {
    Bytes { divisor: u64, name: &'static str },
    Blocks,
}

impl Units {
    /// The largest binary unit that still shows at least ten of itself for `reference`.
    fn for_bytes(reference: u64, blocks: bool) -> Self {
        if blocks {
            return Self::Blocks;
        }
        let units = [(1 << 40, "TiB"), (1 << 30, "GiB"), (1 << 20, "MiB")];
        let (divisor, name) = units
            .into_iter()
            .find(|(divisor, _)| reference >= 10 * divisor)
            .unwrap_or((1 << 10, "KiB"));
        Self::Bytes { divisor, name }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Bytes { name, .. } => name,
            Self::Blocks => "4 KiB blocks",
        }
    }

    /// A size without its unit, for table columns.
    fn number(self, bytes: u64) -> String {
        match self {
            Self::Bytes { divisor, .. } => format!("{:.1}", bytes as f64 / divisor as f64),
            Self::Blocks => (bytes / BLOCK_BYTES).to_string(),
        }
    }

    fn size(self, bytes: u64) -> String {
        match self {
            Self::Bytes { name, .. } => format!("{} {name}", self.number(bytes)),
            Self::Blocks => {
                let blocks = bytes / BLOCK_BYTES;
                format!("{blocks} {}", plural(blocks, "block"))
            }
        }
    }

    fn blocks(self, blocks: u64) -> String {
        self.size(blocks * BLOCK_BYTES)
    }
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".to_owned();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// Labelled rows. Sizes right-align in one column so that their digits line up.
struct Rows {
    rows: Vec<(String, String, String)>,
    right_aligned: bool,
}

impl Rows {
    fn sizes() -> Self {
        Self {
            rows: Vec::new(),
            right_aligned: true,
        }
    }

    fn facts() -> Self {
        Self {
            rows: Vec::new(),
            right_aligned: false,
        }
    }

    fn row(&mut self, label: &str, value: String, note: String) {
        self.rows.push((label.to_owned(), value, note));
    }

    fn render(&self, out: &mut String) {
        let width = self.rows.iter().map(|(_, value, _)| value.len()).max();
        let width = width.unwrap_or(0);
        for (label, value, note) in &self.rows {
            let line = if self.right_aligned {
                format!("  {label:<20}{value:>width$}    {note}")
            } else {
                format!("  {label:<20}{value}    {note}")
            };
            let _ = writeln!(out, "{}", line.trim_end());
        }
    }
}

fn render_info(view: &VolumeView, backing: &Path, options: &InfoOptions) -> String {
    let style = options.style;
    let units = Units::for_bytes(
        view.provisioned_blocks
            .max(view.backing.bytes / BLOCK_BYTES)
            * BLOCK_BYTES,
        options.blocks,
    );
    let totals = view.totals();
    let mut out = String::new();
    let kind = match view.backing.kind {
        BackingKind::RegularFile => "regular file",
        BackingKind::BlockDevice => "block device",
    };
    let _ = writeln!(
        out,
        "{}     {:016x}   format {}   block 4 KiB",
        style.bold("volume"),
        view.volume_id,
        view.format_version
    );
    let _ = writeln!(
        out,
        "{}    {}   {kind}\n",
        style.bold("backing"),
        backing.display()
    );
    render_size(&mut out, view, units, style);
    render_log(&mut out, view, &totals, units, style);
    render_segment_summary(&mut out, view, &totals, style);
    render_checkpoints(&mut out, view, style);
    render_histogram(&mut out, view, style);
    if options.segments {
        render_segment_table(&mut out, view, units, style);
    }
    render_root_errors(&mut out, view, options);
    out
}

fn render_size(out: &mut String, view: &VolumeView, units: Units, style: Style) {
    let _ = writeln!(out, "{}", style.bold("size"));
    let mut rows = Rows::sizes();
    let logical = u64::from(view.logical_blocks);
    rows.row("logical", units.blocks(logical), String::new());
    let mapped = format!("{} of logical", percent(view.mapped_blocks, logical));
    rows.row("mapped", units.blocks(view.mapped_blocks), mapped);
    rows.row(
        "provisioned",
        units.blocks(view.provisioned_blocks),
        String::new(),
    );
    rows.row("device", units.size(view.backing.bytes), String::new());
    let host = view
        .backing
        .host_allocated_bytes
        .map_or_else(|| "-".to_owned(), |bytes| units.size(bytes));
    rows.row("host allocated", host, String::new());
    rows.render(out);
    out.push('\n');
}

fn render_log(out: &mut String, view: &VolumeView, totals: &LogTotals, units: Units, style: Style) {
    let segment_blocks = u64::from(view.blocks_per_segment);
    let count = view.segments.len() as u64;
    let _ = writeln!(
        out,
        "{}        {count} segments × {} = {}",
        style.bold("log"),
        units.blocks(segment_blocks),
        units.blocks(count * segment_blocks)
    );
    let mut rows = Rows::sizes();
    let sealed = totals.sealed_segments + totals.pending_free_segments;
    let used_note = format!(
        "{sealed} sealed or pending-free, {} open",
        totals.open_segments
    );
    rows.row("used", units.blocks(totals.used_blocks), used_note);
    if let Some(segment) = totals.high_water_segment {
        let end = view.segment_offset(segment) + segment_blocks * BLOCK_BYTES;
        rows.row(
            "high-water mark",
            units.size(end),
            format!("end of segment {segment}"),
        );
    }
    let used = totals.used_blocks;
    let live_note = format!("{} of used", percent(totals.live_blocks, used));
    rows.row("live", units.blocks(totals.live_blocks), live_note);
    rows.row(
        "metadata",
        units.blocks(totals.metadata_blocks),
        String::new(),
    );
    let dead_note = format!("{} of used", percent(totals.dead_blocks, used));
    rows.row("dead", units.blocks(totals.dead_blocks), dead_note);
    let free_note = format!(
        "{} segments + {} in open segments",
        totals.free_segments,
        units.blocks(totals.open_free_blocks)
    );
    rows.row("free", units.blocks(totals.free_blocks), free_note);
    rows.render(out);
    out.push('\n');
}

fn render_segment_summary(out: &mut String, view: &VolumeView, totals: &LogTotals, style: Style) {
    let _ = writeln!(
        out,
        "{}   sealed {}   pending-free {}   open {}   free {}",
        style.bold("segments"),
        totals.sealed_segments,
        totals.pending_free_segments,
        totals.open_segments,
        totals.free_segments
    );
    let mut rows = Rows::facts();
    for head in HeadId::ALL {
        let segment = view
            .head_segment(head)
            .map_or_else(|| "-".to_owned(), |segment| format!("segment {segment}"));
        rows.row(&format!("{} head", head.name()), segment, String::new());
    }
    let oldest = view
        .segments
        .iter()
        .filter_map(|segment| segment.seal_lsn)
        .min()
        .map_or_else(|| "-".to_owned(), |lsn| lsn.to_string());
    rows.row("oldest seal LSN", oldest, String::new());
    rows.render(out);
    out.push('\n');
}

fn render_checkpoints(out: &mut String, view: &VolumeView, style: Style) {
    let _ = writeln!(out, "{}", style.bold("checkpoints"));
    for slot in CheckpointSlot::BOTH {
        let status = match &view.roots[slot.index()] {
            RootStatus::Valid { checkpoint_lsn } => {
                format!("LSN {checkpoint_lsn}   {}", style.green("valid"))
            }
            RootStatus::Invalid { .. } => format!("-   {}", style.red("invalid")),
        };
        let selected = if slot == view.selected {
            format!("   {}", style.bold("selected"))
        } else {
            String::new()
        };
        let _ = writeln!(out, "  {:<20}{status}{selected}", slot.name());
    }
    let _ = writeln!(out, "  {:<20}{}", "tail", tail_summary(&view.tail));
    if view.restarts > 0 {
        let _ = writeln!(out, "  {:<20}{}", "walk restarts", view.restarts);
    }
    out.push('\n');
}

fn tail_summary(tail: &[TailSummary; 2]) -> String {
    let heads = HeadId::ALL.map(|head| {
        let summary = &tail[head as usize];
        match &summary.lsn_range {
            Some(range) => format!(
                "{} {} records (LSN {}..{})",
                head.name(),
                summary.records,
                range.start(),
                range.end()
            ),
            None => format!("{} 0", head.name()),
        }
    });
    heads.join(", ")
}

fn render_histogram(out: &mut String, view: &VolumeView, style: Style) {
    let mut buckets = [0_u64; 10];
    let segment_blocks = u64::from(view.blocks_per_segment).max(1);
    for segment in view
        .segments
        .iter()
        .filter(|segment| segment.seal_lsn.is_some())
    {
        let tenths = u64::from(segment.live_blocks) * 10 / segment_blocks;
        buckets[usize::try_from(tenths.min(9)).unwrap_or(9)] += 1;
    }
    let _ = writeln!(
        out,
        "{}",
        style.bold("live fraction of sealed and pending-free segments")
    );
    let largest = buckets.iter().copied().max().unwrap_or(0).max(1);
    for (decile, count) in (0..).zip(buckets) {
        let label = format!("{}-{}%", decile * 10, decile * 10 + 10);
        let width = usize::try_from((count * HISTOGRAM_WIDTH).div_ceil(largest)).unwrap_or(0);
        let bar = "█".repeat(width);
        let pad = " ".repeat(usize::try_from(HISTOGRAM_WIDTH).unwrap_or(0) - width);
        let _ = writeln!(out, "  {label:>8}  {bar}{pad}  {count}");
    }
}

fn render_segment_table(out: &mut String, view: &VolumeView, units: Units, style: Style) {
    let _ = writeln!(out, "\n{} in {}", style.bold("segment table"), units.name());
    let digits = view.segments.len().max(1).to_string().len().max(3);
    let _ = writeln!(
        out,
        "  {:>digits$}  {:<15}  {:>10}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}",
        "SEG", "STATE", "SEAL_LSN", "USED", "LIVE", "META", "DEAD", "HOST"
    );
    for (segment, view) in view.segments.iter().enumerate() {
        let row = segment_row(view, units);
        let _ = writeln!(out, "  {segment:>digits$}  {row}");
    }
}

fn segment_row(segment: &SegmentView, units: Units) -> String {
    let state = match segment.head {
        Some(head) => format!("open {}", head.name()),
        None => segment.state.name().to_owned(),
    };
    let seal = segment
        .seal_lsn
        .map_or_else(|| "-".to_owned(), |lsn| lsn.to_string());
    let used = u64::from(segment.used_blocks);
    let counts = if segment.state == SegmentState::Free {
        ["-".to_owned(), "-".to_owned(), "-".to_owned()]
    } else {
        let live = u64::from(segment.live_blocks);
        let metadata = u64::from(segment.metadata_blocks);
        let dead = used.saturating_sub(live + metadata);
        [live, metadata, dead].map(|blocks| units.number(blocks * BLOCK_BYTES))
    };
    let host = segment
        .host_allocated_bytes
        .map_or_else(|| "-".to_owned(), |bytes| units.number(bytes));
    format!(
        "{state:<15}  {seal:>10}  {:>7}  {:>7}  {:>7}  {:>7}  {host:>7}",
        units.number(used * BLOCK_BYTES),
        counts[0],
        counts[1],
        counts[2]
    )
}

fn render_root_errors(out: &mut String, view: &VolumeView, options: &InfoOptions) {
    let mut errors = 0_u64;
    for slot in CheckpointSlot::BOTH {
        if let RootStatus::Invalid { reason } = &view.roots[slot.index()] {
            errors += 1;
            if options.verbose || errors <= MAX_ERRORS_SHOWN {
                let line = format!(
                    "error  block {}: {} root: {reason}",
                    slot as u8,
                    slot.name()
                );
                let _ = writeln!(out, "{}", options.style.red(&line));
            }
        }
    }
    if errors > 0 {
        let _ = writeln!(out, "{errors} {}", plural(errors, "error"));
    }
}

fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

fn run_check(backing: &Path, verbose: bool, style: Style) -> u8 {
    let mut shown = 0_u64;
    let mut on_finding = |finding: &Finding| {
        shown += 1;
        if verbose || shown <= MAX_ERRORS_SHOWN {
            emit(&format!("{}\n", style.finding(finding)));
        }
    };
    let report = match block_storage::check(backing, &mut on_finding) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("blkctl: cannot check {}: {error}", backing.display());
            return EXIT_NOT_RUN;
        }
    };
    let mut out = String::new();
    let hidden = shown.saturating_sub(MAX_ERRORS_SHOWN);
    if !verbose && hidden > 0 {
        let _ = writeln!(out, "... {hidden} more not shown; use -v to print them all");
    }
    let _ = writeln!(out, "{}      {}", style.bold("check"), backing.display());
    let mut rows = Rows::facts();
    let root = report.selected.map_or_else(
        || "none valid".to_owned(),
        |slot| format!("{}, checkpoint LSN {}", slot.name(), report.checkpoint_lsn),
    );
    rows.row("root", root, String::new());
    rows.row(
        "segments walked",
        report.segments_walked.to_string(),
        String::new(),
    );
    rows.row("records", report.records.to_string(), String::new());
    rows.row(
        "payloads verified",
        report.payloads.to_string(),
        String::new(),
    );
    rows.row(
        "mapped blocks",
        report.mapped_blocks.to_string(),
        String::new(),
    );
    rows.row("tail", tail_summary(&report.tail), String::new());
    rows.render(&mut out);
    let (code, result) = if report.errors > 0 {
        (
            EXIT_DAMAGED,
            style.red(&count_findings(report.errors, report.warnings)),
        )
    } else if report.has_tail() {
        (
            EXIT_TAIL,
            style.yellow("no errors; the next open replays the tail"),
        )
    } else {
        (EXIT_CLEAN, style.green("no errors"))
    };
    let warnings = if report.errors == 0 && report.warnings > 0 {
        format!(
            ", {}",
            style.yellow(&format!(
                "{} {}",
                report.warnings,
                plural(report.warnings, "warning")
            ))
        )
    } else {
        String::new()
    };
    let _ = writeln!(out, "{}     {result}{warnings}", style.bold("result"));
    emit(&out);
    code
}

fn count_findings(errors: u64, warnings: u64) -> String {
    let mut text = format!("{errors} {}", plural(errors, "error"));
    if warnings > 0 {
        let _ = write!(text, ", {warnings} {}", plural(warnings, "warning"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use block_storage::BackingInfo;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_both_commands_with_options_in_any_order() {
        assert_eq!(
            parse_args(args(&[
                "info",
                "-v",
                "disk.img",
                "--segments",
                "--color=never"
            ])),
            Ok(Command::Info {
                backing: PathBuf::from("disk.img"),
                segments: true,
                blocks: false,
                verbose: true,
                color: ColorChoice::Never,
            })
        );
        assert_eq!(
            parse_args(args(&["check", "--color=always", "/dev/sdb"])),
            Ok(Command::Check {
                backing: PathBuf::from("/dev/sdb"),
                verbose: false,
                color: ColorChoice::Always,
            })
        );
        assert_eq!(parse_args(args(&["--help"])), Ok(Command::Help));
        assert_eq!(parse_args(args(&["info", "-h"])), Ok(Command::Help));
    }

    #[test]
    fn rejects_each_class_of_invalid_arguments() {
        for invalid in [
            &[][..],
            &["mount", "disk.img"],
            &["info"],
            &["info", "a.img", "b.img"],
            &["check", "--segments", "disk.img"],
            &["check", "--blocks", "disk.img"],
            &["info", "--color=sometimes", "disk.img"],
            &["info", "--bogus", "disk.img"],
        ] {
            assert!(parse_args(args(invalid)).is_err(), "{invalid:?}");
        }
    }

    fn segment(state: SegmentState, live: u32) -> SegmentView {
        SegmentView {
            state,
            head: None,
            seal_lsn: state.has_seal().then_some(1000 + u64::from(live)),
            used_blocks: if state == SegmentState::Free { 0 } else { 2048 },
            live_blocks: live,
            metadata_blocks: u32::from(state != SegmentState::Free),
            host_allocated_bytes: Some(u64::from(live) * BLOCK_BYTES),
        }
    }

    /// 128 MiB on 185 MiB with 8 MiB segments, as in the ADR's example.
    fn example_view() -> VolumeView {
        let mut segments = vec![segment(SegmentState::Free, 0); 23];
        for (index, live) in [2000, 150, 2048, 1800, 900, 1500].into_iter().enumerate() {
            segments[index] = segment(SegmentState::Sealed, live);
        }
        segments[6] = segment(SegmentState::PendingFree, 0);
        segments[7] = SegmentView {
            head: Some(HeadId::User),
            state: SegmentState::Open,
            seal_lsn: None,
            used_blocks: 1024,
            metadata_blocks: 1,
            ..segments[7]
        };
        segments[12] = SegmentView {
            head: Some(HeadId::Relocation),
            state: SegmentState::Open,
            seal_lsn: None,
            used_blocks: 10,
            metadata_blocks: 1,
            ..segments[12]
        };
        VolumeView {
            volume_id: 0x3f9a_1c07_5e2b_40d1,
            format_version: 4,
            backing: BackingInfo {
                kind: BackingKind::RegularFile,
                bytes: 185 << 20,
                host_allocated_bytes: Some(131 << 20),
            },
            logical_blocks: 32_768,
            provisioned_blocks: (185 << 20) / BLOCK_BYTES,
            blocks_per_segment: 2048,
            roots: [
                RootStatus::Valid {
                    checkpoint_lsn: 48_213,
                },
                RootStatus::Invalid {
                    reason: "metadata checksum mismatch".to_owned(),
                },
            ],
            selected: CheckpointSlot::Green,
            restarts: 1,
            segments: segments.into_boxed_slice(),
            mapped_blocks: 8398,
            tail: [
                TailSummary {
                    records: 37,
                    lsn_range: Some(48_214..=48_251),
                },
                TailSummary::default(),
            ],
        }
    }

    fn render(segments: bool, blocks: bool, color: bool) -> String {
        let options = InfoOptions {
            segments,
            blocks,
            verbose: false,
            style: Style { enabled: color },
        };
        render_info(&example_view(), Path::new("disk.img"), &options)
    }

    #[test]
    fn info_summary_fits_80_columns_and_names_every_section() {
        let text = render(true, false, false);
        for line in text.lines() {
            assert!(line.chars().count() <= 80, "{} columns: {line}", line.len());
        }
        for expected in [
            "volume     3f9a1c075e2b40d1   format 4   block 4 KiB",
            "  logical             128.0 MiB",
            "  mapped               32.8 MiB    25.6% of logical",
            "log        23 segments × 8.0 MiB = 184.0 MiB",
            "  high-water mark     104.0 MiB    end of segment 12",
            "segments   sealed 6   pending-free 1   open 2   free 14",
            "  user head           segment 7",
            "  relocation head     segment 12",
            "  oldest seal LSN     1000",
            "  green               LSN 48213   valid   selected",
            "  blue                -   invalid",
            "  tail                user 37 records (LSN 48214..48251), relocation 0",
            "  walk restarts       1",
            "   90-100%  ████",
            "error  block 1: blue root: metadata checksum mismatch",
            "1 error",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn segment_table_and_block_units() {
        let text = render(true, true, false);
        assert!(text.contains("segment table in 4 KiB blocks"), "{text}");
        assert!(
            text.contains("  logical             32768 blocks"),
            "{text}"
        );
        let user = text
            .lines()
            .find(|line| line.contains("open user"))
            .expect("user head row");
        assert_eq!(
            user.split_whitespace().collect::<Vec<_>>(),
            ["7", "open", "user", "-", "1024", "0", "1", "1023", "0"]
        );
        let free = text
            .lines()
            .find(|line| line.trim_start().starts_with("8  free"))
            .expect("free row");
        assert!(
            free.ends_with("0        -        -        -        0"),
            "{free}"
        );
    }

    #[test]
    fn color_is_ansi_and_only_when_enabled() {
        let colored = render(false, false, true);
        assert!(colored.contains("\x1b[1mvolume\x1b[0m"));
        assert!(colored.contains("\x1b[32mvalid\x1b[0m"));
        assert!(colored.contains("\x1b[31minvalid\x1b[0m"));
        assert!(!render(false, false, false).contains('\x1b'));
        assert!(!Style::new(ColorChoice::Never).enabled);
        assert!(Style::new(ColorChoice::Always).enabled);
    }

    #[test]
    fn units_pick_the_largest_unit_with_ten_of_it() {
        assert_eq!(Units::for_bytes(185 << 20, false).name(), "MiB");
        assert_eq!(Units::for_bytes(4 << 30, false).name(), "MiB");
        assert_eq!(Units::for_bytes(500 << 30, false).name(), "GiB");
        assert_eq!(Units::for_bytes(1 << 20, false).name(), "KiB");
        assert_eq!(Units::for_bytes(1 << 40, true), Units::Blocks);
        assert_eq!(
            Units::for_bytes(185 << 20, false).size(0x66_6666),
            "6.4 MiB"
        );
        assert_eq!(percent(1, 3), "33.3%");
        assert_eq!(percent(1, 0), "-");
    }
}
