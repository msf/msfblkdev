use super::*;

// Linux CPU topology and memory metadata fit comfortably within this read bound.
const MAX_METADATA_BYTES: u64 = 1024 * 1024;

pub(super) fn read() -> io::Result<Value> {
    parse(
        &bounded_text("/proc/cpuinfo")?,
        &bounded_text("/sys/devices/system/cpu/online")?,
        &bounded_text("/proc/meminfo")?,
    )
}

fn bounded_text(path: &str) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_METADATA_BYTES {
        return Err(io::Error::other("hardware metadata exceeds read bound"));
    }
    Ok(text)
}

fn parse(cpu: &str, online: &str, memory: &str) -> io::Result<Value> {
    let field = |text: &str, key: &str| {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim() == key).then(|| value.trim().to_owned())
        })
    };
    let model = field(cpu, "model name")
        .or_else(|| field(cpu, "Hardware"))
        .or_else(|| {
            // arm64 /proc/cpuinfo may report MIDR fields instead of a model name. Keep the
            // implementer and part from the same processor; neither is a marketing name.
            cpu.split("\n\n").find_map(|core| {
                let implementer = field(core, "CPU implementer")?;
                let part = field(core, "CPU part")?;
                let implementer = u8::from_str_radix(implementer.strip_prefix("0x")?, 16).ok()?;
                let part = u16::from_str_radix(part.strip_prefix("0x")?, 16).ok()?;
                (part <= 0xfff)
                    .then(|| format!("CPU implementer 0x{implementer:02x} part 0x{part:03x}"))
            })
        })
        .ok_or_else(|| io::Error::other("CPU model is unavailable"))?;
    let memory =
        field(memory, "MemTotal").ok_or_else(|| io::Error::other("memory size is unavailable"))?;
    let kib = memory
        .strip_suffix(" kB")
        .ok_or_else(|| io::Error::other("unexpected memory unit"))?
        .trim()
        .parse::<u64>()
        .map_err(io::Error::other)?;
    let bytes = kib
        .checked_mul(1024)
        .filter(|value| *value != 0)
        .ok_or_else(|| io::Error::other("invalid memory size"))?;
    if online.trim().is_empty() || model.is_empty() {
        return Err(io::Error::other("hardware metadata is empty"));
    }
    Ok(json!({"cpu_model": model, "online_cpus": online.trim(), "memory_bytes": bytes}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_arm_cpu_fields_without_inventing_a_model_name() {
        let memory = "MemTotal: 1024 kB";
        let cpu = "processor: 0\nCPU implementer: 0x61\nCPU part: 0x000";
        assert_eq!(
            parse(cpu, "0", memory).unwrap()["cpu_model"],
            "CPU implementer 0x61 part 0x000"
        );
        assert_eq!(
            parse(&format!("model name: known\n{cpu}"), "0", memory).unwrap()["cpu_model"],
            "known"
        );
        for invalid in [
            "CPU implementer: 0x61",
            "CPU implementer: 0x61\n\nCPU part: 0x000",
            "CPU implementer: nope\nCPU part: 0x000",
            "CPU implementer: 0x61\nCPU part: 0x1000",
        ] {
            assert!(parse(invalid, "0", memory).is_err());
        }
    }

    #[test]
    fn records_stable_hardware_not_free_memory_or_frequency() {
        let value = parse(
            "model name: example\ncpu MHz: 1000",
            "0-3\n",
            "MemTotal: 1024 kB",
        )
        .unwrap();
        assert_eq!(
            value,
            json!({"cpu_model":"example", "online_cpus":"0-3", "memory_bytes":1048576})
        );
        assert!(parse("", "0", "MemTotal: 1024 kB").is_err());
        assert!(parse("model name: example", "0", "MemTotal: invalid kB").is_err());
    }
}
