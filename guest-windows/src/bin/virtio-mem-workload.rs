use std::io::{self, Write};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const USAGE: &str = "Usage: virtio-mem-workload \
    --workload-id ID \
    --mode committed|resident \
    --peak-bytes BYTES \
    --retained-bytes BYTES \
    --max-allocation-bytes BYTES \
    --peak-hold-seconds SECONDS \
    --settled-hold-seconds SECONDS \
    --renewed-hold-seconds SECONDS \
    --refresh-interval-seconds SECONDS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkloadMode {
    Committed,
    Resident,
}

impl WorkloadMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "committed" => Ok(Self::Committed),
            "resident" => Ok(Self::Resident),
            _ => Err("--mode must be committed or resident".to_owned()),
        }
    }

    fn touches_pages(self) -> bool {
        self == Self::Resident
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    workload_id: String,
    mode: WorkloadMode,
    peak_bytes: u64,
    retained_bytes: u64,
    max_allocation_bytes: u64,
    peak_hold: Duration,
    settled_hold: Duration,
    renewed_hold: Duration,
    refresh_interval: Duration,
}

impl Config {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        if arguments.len() != 18 {
            return Err(USAGE.to_owned());
        }

        let mut workload_id = None;
        let mut mode = None;
        let mut peak_bytes = None;
        let mut retained_bytes = None;
        let mut max_allocation_bytes = None;
        let mut peak_hold = None;
        let mut settled_hold = None;
        let mut renewed_hold = None;
        let mut refresh_interval = None;

        for pair in arguments.chunks_exact(2) {
            let slot = pair[0].as_str();
            let value = pair[1].as_str();
            match slot {
                "--workload-id" => set_once(&mut workload_id, parse_id(value)?, slot)?,
                "--mode" => set_once(&mut mode, WorkloadMode::parse(value)?, slot)?,
                "--peak-bytes" => set_once(&mut peak_bytes, parse_u64(value, slot)?, slot)?,
                "--retained-bytes" => set_once(&mut retained_bytes, parse_u64(value, slot)?, slot)?,
                "--max-allocation-bytes" => {
                    set_once(&mut max_allocation_bytes, parse_u64(value, slot)?, slot)?
                }
                "--peak-hold-seconds" => {
                    set_once(&mut peak_hold, parse_hold_duration(value, slot)?, slot)?
                }
                "--settled-hold-seconds" => {
                    set_once(&mut settled_hold, parse_hold_duration(value, slot)?, slot)?
                }
                "--renewed-hold-seconds" => {
                    set_once(&mut renewed_hold, parse_hold_duration(value, slot)?, slot)?
                }
                "--refresh-interval-seconds" => set_once(
                    &mut refresh_interval,
                    parse_hold_duration(value, slot)?,
                    slot,
                )?,
                _ => return Err(format!("unknown option: {slot}\n{USAGE}")),
            }
        }

        let config = Self {
            workload_id: required(workload_id, "--workload-id")?,
            mode: required(mode, "--mode")?,
            peak_bytes: required(peak_bytes, "--peak-bytes")?,
            retained_bytes: required(retained_bytes, "--retained-bytes")?,
            max_allocation_bytes: required(max_allocation_bytes, "--max-allocation-bytes")?,
            peak_hold: required(peak_hold, "--peak-hold-seconds")?,
            settled_hold: required(settled_hold, "--settled-hold-seconds")?,
            renewed_hold: required(renewed_hold, "--renewed-hold-seconds")?,
            refresh_interval: required(refresh_interval, "--refresh-interval-seconds")?,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.max_allocation_bytes == 0 {
            return Err("--max-allocation-bytes must be positive".to_owned());
        }
        if self.peak_bytes == 0 || self.peak_bytes > self.max_allocation_bytes {
            return Err(format!(
                "--peak-bytes must be positive and no greater than --max-allocation-bytes ({})",
                self.max_allocation_bytes
            ));
        }
        if self.retained_bytes == 0 || self.retained_bytes >= self.peak_bytes {
            return Err("--retained-bytes must be positive and below --peak-bytes".to_owned());
        }
        Ok(())
    }

    fn releasable_bytes(&self) -> u64 {
        self.peak_bytes - self.retained_bytes
    }
}

fn parse_id(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(
            "--workload-id must contain 1..=64 ASCII letters, digits, '.', '-', or '_'".to_owned(),
        );
    }
    Ok(value.to_owned())
}

fn parse_u64(value: &str, option: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{option} must be an unsigned integer"))
}

fn parse_hold_duration(value: &str, option: &str) -> Result<Duration, String> {
    let seconds = parse_u64(value, option)?;
    if seconds == 0 {
        return Err(format!("{option} must be positive"));
    }
    Ok(Duration::from_secs(seconds))
}

fn required<T>(value: Option<T>, option: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("missing required option: {option}\n{USAGE}"))
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("duplicate option: {option}"));
    }
    *slot = Some(value);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Baseline,
    Peak,
    Settled,
    Renewed,
    Complete,
}

#[derive(Debug, Serialize)]
struct Evidence<'a> {
    version: u32,
    workload_id: &'a str,
    mode: WorkloadMode,
    phase: Phase,
    unix_millis: u128,
    elapsed_millis: u128,
    committed_bytes: u64,
    touched_bytes: u64,
}

fn emit(
    config: &Config,
    phase: Phase,
    started: Instant,
    committed_bytes: u64,
) -> Result<(), String> {
    let unix_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock precedes Unix epoch: {error}"))?
        .as_millis();
    let evidence = Evidence {
        version: 1,
        workload_id: &config.workload_id,
        mode: config.mode,
        phase,
        unix_millis,
        elapsed_millis: started.elapsed().as_millis(),
        committed_bytes,
        touched_bytes: if config.mode.touches_pages() {
            committed_bytes
        } else {
            0
        },
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &evidence)
        .map_err(|error| format!("failed to serialize workload evidence: {error}"))?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(|error| format!("failed to write workload evidence: {error}"))
}

#[cfg(windows)]
mod native {
    use std::mem::{size_of, zeroed};
    use std::ptr::{null_mut, write_volatile};

    use winapi::ctypes::c_void;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::memoryapi::{VirtualAlloc, VirtualFree};
    use winapi::um::psapi::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    use winapi::um::winnt::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};

    pub(super) struct Allocation {
        address: *mut c_void,
        bytes: usize,
        page_size: usize,
        touch: bool,
    }

    impl Allocation {
        pub(super) fn new(bytes: u64, touch: bool) -> Result<Self, String> {
            let bytes = usize::try_from(bytes)
                .map_err(|_| "allocation size exceeds this process address space".to_owned())?;
            let page_size = system_page_size()?;
            if bytes % page_size != 0 {
                return Err(format!(
                    "allocation size {bytes} is not aligned to Windows page size {page_size}"
                ));
            }
            // SAFETY: The null address asks Windows to choose a region. The requested
            // committed, private, read/write mapping is owned by this value and is
            // released exactly once in Drop.
            let address = unsafe {
                VirtualAlloc(null_mut(), bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
            };
            if address.is_null() {
                return Err(format!(
                    "VirtualAlloc({bytes}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut allocation = Self {
                address,
                bytes,
                page_size,
                touch,
            };
            allocation.refresh();
            Ok(allocation)
        }

        pub(super) fn refresh(&mut self) {
            if !self.touch {
                return;
            }
            let base = self.address.cast::<u8>();
            for offset in (0..self.bytes).step_by(self.page_size) {
                // SAFETY: Every offset is within this value's committed mapping. A
                // volatile byte write faults in one byte from each system page.
                unsafe { write_volatile(base.add(offset), 1) };
            }
        }
    }

    impl Drop for Allocation {
        fn drop(&mut self) {
            // SAFETY: address is the base returned by VirtualAlloc and zero is
            // required with MEM_RELEASE. Windows owns no reference after return.
            let released = unsafe { VirtualFree(self.address, 0, MEM_RELEASE) };
            if released == FALSE {
                eprintln!(
                    "virtio-mem-workload: VirtualFree failed during cleanup: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    fn system_page_size() -> Result<usize, String> {
        // SAFETY: GetPerformanceInfo initializes the correctly sized structure and
        // does not retain the supplied pointer.
        let information = unsafe {
            let mut value: PERFORMANCE_INFORMATION = zeroed();
            if GetPerformanceInfo(&mut value, size_of::<PERFORMANCE_INFORMATION>() as u32) == FALSE
            {
                return Err(format!(
                    "GetPerformanceInfo failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            value
        };
        let page_size = information.PageSize as usize;
        if page_size == 0 {
            return Err("GetPerformanceInfo returned a zero page size".to_owned());
        }
        Ok(page_size)
    }
}

#[cfg(windows)]
fn hold(
    duration: Duration,
    refresh_interval: Duration,
    allocations: &mut [&mut native::Allocation],
) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(duration)
        .ok_or_else(|| "workload hold duration exceeds the platform clock range".to_owned())?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(refresh_interval));
        for allocation in allocations.iter_mut() {
            allocation.refresh();
        }
    }
}

#[cfg(windows)]
fn run(config: &Config) -> Result<(), String> {
    let started = Instant::now();
    emit(config, Phase::Baseline, started, 0)?;

    let mut retained = native::Allocation::new(config.retained_bytes, config.mode.touches_pages())?;
    let mut releasable =
        native::Allocation::new(config.releasable_bytes(), config.mode.touches_pages())?;
    emit(config, Phase::Peak, started, config.peak_bytes)?;
    hold(
        config.peak_hold,
        config.refresh_interval,
        &mut [&mut retained, &mut releasable],
    )?;

    drop(releasable);
    emit(config, Phase::Settled, started, config.retained_bytes)?;
    hold(
        config.settled_hold,
        config.refresh_interval,
        &mut [&mut retained],
    )?;

    releasable = native::Allocation::new(config.releasable_bytes(), config.mode.touches_pages())?;
    emit(config, Phase::Renewed, started, config.peak_bytes)?;
    hold(
        config.renewed_hold,
        config.refresh_interval,
        &mut [&mut retained, &mut releasable],
    )?;

    drop(releasable);
    drop(retained);
    emit(config, Phase::Complete, started, 0)
}

#[cfg(not(windows))]
fn run(_config: &Config) -> Result<(), String> {
    Err("virtio-mem-workload is supported only on Windows".to_owned())
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        arguments.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match Config::parse(&arguments).and_then(|config| run(&config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("virtio-mem-workload: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_arguments() -> Vec<String> {
        [
            "--workload-id",
            "resident-cycle",
            "--mode",
            "resident",
            "--peak-bytes",
            "8192",
            "--retained-bytes",
            "4096",
            "--max-allocation-bytes",
            "16384",
            "--peak-hold-seconds",
            "2",
            "--settled-hold-seconds",
            "3",
            "--renewed-hold-seconds",
            "2",
            "--refresh-interval-seconds",
            "1",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn parses_an_explicit_bounded_workload() {
        let config = Config::parse(&valid_arguments()).expect("valid workload");

        assert_eq!(config.workload_id, "resident-cycle");
        assert_eq!(config.mode, WorkloadMode::Resident);
        assert_eq!(config.peak_bytes, 8192);
        assert_eq!(config.retained_bytes, 4096);
        assert_eq!(config.releasable_bytes(), 4096);
    }

    #[test]
    fn accepts_arguments_in_any_order() {
        let mut arguments = valid_arguments();
        arguments.rotate_left(4);

        assert!(Config::parse(&arguments).is_ok());
    }

    #[test]
    fn rejects_duplicate_and_missing_options() {
        let mut duplicate = valid_arguments();
        duplicate[0] = "--mode".to_owned();
        duplicate[1] = "committed".to_owned();
        assert!(Config::parse(&duplicate)
            .expect_err("duplicate mode")
            .contains("duplicate option: --mode"));

        let mut missing = valid_arguments();
        missing.truncate(16);
        assert_eq!(Config::parse(&missing), Err(USAGE.to_owned()));
    }

    #[test]
    fn rejects_unbounded_or_inverted_memory() {
        let mut too_large = valid_arguments();
        too_large[5] = "16385".to_owned();
        assert!(Config::parse(&too_large).is_err());

        let mut inverted = valid_arguments();
        inverted[7] = "8192".to_owned();
        assert!(Config::parse(&inverted).is_err());
    }

    #[test]
    fn rejects_unsafe_identity_and_zero_holds() {
        let mut unsafe_id = valid_arguments();
        unsafe_id[1] = "bad id&command".to_owned();
        assert!(Config::parse(&unsafe_id).is_err());

        let mut zero_hold = valid_arguments();
        zero_hold[11] = "0".to_owned();
        assert!(Config::parse(&zero_hold).is_err());
    }

    #[test]
    fn evidence_distinguishes_committed_only_from_page_touched_memory() {
        assert!(!WorkloadMode::Committed.touches_pages());
        assert!(WorkloadMode::Resident.touches_pages());
    }

    #[test]
    fn serializes_versioned_correlatable_evidence() {
        let evidence = Evidence {
            version: 1,
            workload_id: "committed-cycle",
            mode: WorkloadMode::Committed,
            phase: Phase::Settled,
            unix_millis: 1_000,
            elapsed_millis: 500,
            committed_bytes: 4096,
            touched_bytes: 0,
        };

        let value = serde_json::to_value(evidence).expect("serialize evidence");
        assert_eq!(value["version"], 1);
        assert_eq!(value["workload_id"], "committed-cycle");
        assert_eq!(value["mode"], "committed");
        assert_eq!(value["phase"], "settled");
        assert_eq!(value["committed_bytes"], 4096);
        assert_eq!(value["touched_bytes"], 0);
    }
}
