//! The Linux qualification workload. It takes the Windows workload's options,
//! so the same qualification plan drives both, and adds a file-cache mode,
//! because how the guest treats page cache is the Linux-specific risk.
//!
//! Phases: baseline, peak (hold `--peak-bytes`), settled (keep
//! `--retained-bytes`), renewed (peak again), complete. Each phase start is a
//! JSON line on standard output.
//!
//! - resident: anonymous memory, every page touched and touched again each
//!   refresh interval, so it stays in RAM.
//! - committed: anonymous memory mapped but never touched. Under the default
//!   overcommit it uses no RAM; under strict overcommit it uses commit.
//! - file-cache: files of the phase's size in `--cache-dir`, read through
//!   each refresh interval, so their page cache is the working set.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "Usage: rampire-workload \
    --workload-id ID \
    --mode committed|resident|file-cache \
    --peak-bytes BYTES \
    --retained-bytes BYTES \
    --max-allocation-bytes BYTES \
    --peak-hold-seconds SECONDS \
    --settled-hold-seconds SECONDS \
    --renewed-hold-seconds SECONDS \
    --refresh-interval-seconds SECONDS \
    [--cache-dir DIR]";

/// File-cache data is written in files of this size.
const FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Committed,
    Resident,
    FileCache,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Resident => "resident",
            Self::FileCache => "file_cache",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    workload_id: String,
    mode: Mode,
    peak_bytes: u64,
    retained_bytes: u64,
    max_allocation_bytes: u64,
    peak_hold: Duration,
    settled_hold: Duration,
    renewed_hold: Duration,
    refresh_interval: Duration,
    cache_dir: PathBuf,
}

fn parse(arguments: &[String]) -> Result<Config, String> {
    if !arguments.len().is_multiple_of(2) {
        return Err(USAGE.to_owned());
    }
    let mut values = std::collections::BTreeMap::new();
    for pair in arguments.as_chunks::<2>().0 {
        let known = [
            "--workload-id",
            "--mode",
            "--peak-bytes",
            "--retained-bytes",
            "--max-allocation-bytes",
            "--peak-hold-seconds",
            "--settled-hold-seconds",
            "--renewed-hold-seconds",
            "--refresh-interval-seconds",
            "--cache-dir",
        ];
        if !known.contains(&pair[0].as_str()) {
            return Err(format!("unknown option: {}\n{USAGE}", pair[0]));
        }
        if values.insert(pair[0].as_str(), pair[1].as_str()).is_some() {
            return Err(format!("duplicate option: {}", pair[0]));
        }
    }
    let get = |option: &str| {
        values
            .get(option)
            .copied()
            .ok_or_else(|| format!("missing required option: {option}\n{USAGE}"))
    };
    let number = |option: &str| -> Result<u64, String> {
        get(option)?
            .parse()
            .map_err(|_| format!("{option} must be an unsigned integer"))
    };
    let seconds = |option: &str| -> Result<Duration, String> {
        match number(option)? {
            0 => Err(format!("{option} must be positive")),
            value => Ok(Duration::from_secs(value)),
        }
    };
    let workload_id = get("--workload-id")?.to_owned();
    if workload_id.is_empty()
        || workload_id.len() > 64
        || !workload_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        return Err(
            "--workload-id must contain 1..=64 ASCII letters, digits, '.', '-', or '_'".to_owned(),
        );
    }
    let mode = match get("--mode")? {
        "committed" => Mode::Committed,
        "resident" => Mode::Resident,
        "file-cache" => Mode::FileCache,
        _ => return Err("--mode must be committed, resident or file-cache".to_owned()),
    };
    let cache_dir = match values.get("--cache-dir") {
        Some(_) if mode != Mode::FileCache => {
            return Err("--cache-dir applies only to --mode file-cache".to_owned())
        }
        Some(path) if path.starts_with('/') => PathBuf::from(path),
        Some(_) => return Err("--cache-dir must be an absolute path".to_owned()),
        None => PathBuf::from(format!("/var/tmp/rampire-workload-{workload_id}")),
    };
    let config = Config {
        workload_id,
        mode,
        peak_bytes: number("--peak-bytes")?,
        retained_bytes: number("--retained-bytes")?,
        max_allocation_bytes: number("--max-allocation-bytes")?,
        peak_hold: seconds("--peak-hold-seconds")?,
        settled_hold: seconds("--settled-hold-seconds")?,
        renewed_hold: seconds("--renewed-hold-seconds")?,
        refresh_interval: seconds("--refresh-interval-seconds")?,
        cache_dir,
    };
    if config.max_allocation_bytes == 0
        || config.peak_bytes == 0
        || config.peak_bytes > config.max_allocation_bytes
    {
        return Err(
            "--peak-bytes must be positive and no greater than --max-allocation-bytes".to_owned(),
        );
    }
    if config.retained_bytes == 0 || config.retained_bytes >= config.peak_bytes {
        return Err("--retained-bytes must be positive and below --peak-bytes".to_owned());
    }
    Ok(config)
}

fn emit(config: &Config, phase: &str, started: Instant, held_bytes: u64) -> Result<(), String> {
    let unix_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock precedes the Unix epoch: {error}"))?
        .as_millis();
    let line = serde_json::json!({
        "version": 1,
        "workload_id": config.workload_id,
        "mode": config.mode.name(),
        "phase": phase,
        "unix_millis": unix_millis,
        "elapsed_millis": started.elapsed().as_millis(),
        "held_bytes": held_bytes,
    });
    let mut output = io::stdout().lock();
    writeln!(output, "{line}")
        .and_then(|()| output.flush())
        .map_err(|error| format!("write workload evidence: {error}"))
}

/// What a phase holds: anonymous memory, or files whose cache is kept warm.
trait Load {
    fn resize(&mut self, bytes: u64) -> Result<(), String>;
    fn refresh(&mut self) -> Result<(), String>;
}

/// Anonymous memory, touched or not.
struct Anonymous {
    regions: Vec<memory::Region>,
    touch: bool,
}

impl Load for Anonymous {
    fn resize(&mut self, bytes: u64) -> Result<(), String> {
        self.regions.clear();
        // One region per phase keeps the release whole when it shrinks.
        self.regions.push(memory::Region::new(bytes, self.touch)?);
        Ok(())
    }

    fn refresh(&mut self) -> Result<(), String> {
        for region in &mut self.regions {
            region.touch();
        }
        Ok(())
    }
}

/// Files of `FILE_BYTES` each in `directory`, read through on refresh.
struct FileCache {
    directory: PathBuf,
    files: Vec<PathBuf>,
}

impl FileCache {
    fn file(&self, index: usize) -> PathBuf {
        self.directory.join(format!("data-{index:05}"))
    }
}

impl Load for FileCache {
    fn resize(&mut self, bytes: u64) -> Result<(), String> {
        std::fs::create_dir_all(&self.directory)
            .map_err(|error| format!("create {}: {error}", self.directory.display()))?;
        let wanted = usize::try_from(bytes.div_ceil(FILE_BYTES))
            .map_err(|_| "file-cache size is too large".to_owned())?;
        while self.files.len() > wanted {
            let path = self.files.pop().expect("a file to remove");
            std::fs::remove_file(&path)
                .map_err(|error| format!("remove {}: {error}", path.display()))?;
        }
        let block = vec![0x5a_u8; 1024 * 1024];
        while self.files.len() < wanted {
            let path = self.file(self.files.len());
            let mut file = std::fs::File::create(&path)
                .map_err(|error| format!("create {}: {error}", path.display()))?;
            for _ in 0..FILE_BYTES / block.len() as u64 {
                file.write_all(&block)
                    .map_err(|error| format!("write {}: {error}", path.display()))?;
            }
            self.files.push(path);
        }
        self.refresh()
    }

    fn refresh(&mut self) -> Result<(), String> {
        let mut buffer = vec![0_u8; 1024 * 1024];
        for path in &self.files {
            let mut file = std::fs::File::open(path)
                .map_err(|error| format!("open {}: {error}", path.display()))?;
            while file
                .read(&mut buffer)
                .map_err(|error| format!("read {}: {error}", path.display()))?
                > 0
            {}
        }
        Ok(())
    }
}

impl Drop for FileCache {
    fn drop(&mut self) {
        // Best effort: the workload's result is already reported.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[cfg(unix)]
mod memory {
    use std::ptr::{null_mut, write_volatile};

    /// A private anonymous mapping, unmapped on drop.
    pub struct Region {
        address: *mut u8,
        bytes: usize,
        page: usize,
    }

    impl Region {
        pub fn new(bytes: u64, touch: bool) -> Result<Self, String> {
            let bytes = usize::try_from(bytes)
                .map_err(|_| "allocation exceeds this process's address space".to_owned())?;
            // SAFETY: sysconf has no preconditions.
            let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
                .ok()
                .filter(|page| *page > 0)
                .unwrap_or(4096);
            // SAFETY: a new private anonymous mapping, owned by this value and
            // unmapped exactly once in Drop.
            let address = unsafe {
                libc::mmap(
                    null_mut(),
                    bytes,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if address == libc::MAP_FAILED {
                return Err(format!(
                    "mmap({bytes}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut region = Self {
                address: address.cast(),
                bytes,
                page,
            };
            if touch {
                region.touch();
            }
            Ok(region)
        }

        pub fn touch(&mut self) {
            for offset in (0..self.bytes).step_by(self.page) {
                // SAFETY: every offset lies inside this value's mapping.
                unsafe { write_volatile(self.address.add(offset), 1) };
            }
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: address and bytes are exactly what mmap returned.
            if unsafe { libc::munmap(self.address.cast(), self.bytes) } != 0 {
                eprintln!(
                    "rampire-workload: munmap failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

#[cfg(not(unix))]
mod memory {
    pub struct Region;

    impl Region {
        pub fn new(_bytes: u64, _touch: bool) -> Result<Self, String> {
            Err("the Linux workload runs only on Linux".to_owned())
        }

        pub fn touch(&mut self) {}
    }
}

/// Hold `load` for `hold`, refreshing it every `interval`.
fn hold(load: &mut dyn Load, hold: Duration, interval: Duration) -> Result<(), String> {
    let until = Instant::now() + hold;
    loop {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(interval));
        load.refresh()?;
    }
}

fn run(config: &Config) -> Result<(), String> {
    let started = Instant::now();
    let mut load: Box<dyn Load> = match config.mode {
        Mode::FileCache => Box::new(FileCache {
            directory: config.cache_dir.clone(),
            files: Vec::new(),
        }),
        mode => Box::new(Anonymous {
            regions: Vec::new(),
            touch: mode == Mode::Resident,
        }),
    };
    emit(config, "baseline", started, 0)?;
    for (phase, bytes, duration) in [
        ("peak", config.peak_bytes, config.peak_hold),
        ("settled", config.retained_bytes, config.settled_hold),
        ("renewed", config.peak_bytes, config.renewed_hold),
    ] {
        load.resize(bytes)?;
        emit(config, phase, started, bytes)?;
        hold(load.as_mut(), duration, config.refresh_interval)?;
    }
    drop(load);
    emit(config, "complete", started, 0)
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match parse(&arguments).and_then(|config| run(&config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rampire-workload: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(extra: &[&str]) -> Vec<String> {
        [
            "--workload-id",
            "run-1",
            "--mode",
            "resident",
            "--peak-bytes",
            "2048",
            "--retained-bytes",
            "1024",
            "--max-allocation-bytes",
            "4096",
            "--peak-hold-seconds",
            "1",
            "--settled-hold-seconds",
            "1",
            "--renewed-hold-seconds",
            "1",
            "--refresh-interval-seconds",
            "1",
        ]
        .iter()
        .chain(extra)
        .map(|value| (*value).to_owned())
        .collect()
    }

    fn with(option: &str, value: &str) -> Vec<String> {
        let mut values = arguments(&[]);
        let index = values.iter().position(|item| item == option).unwrap();
        values[index + 1] = value.to_owned();
        values
    }

    #[test]
    fn takes_the_windows_options_and_a_file_cache_mode() {
        let config = parse(&arguments(&[])).unwrap();
        assert_eq!(config.mode, Mode::Resident);
        assert_eq!((config.peak_bytes, config.retained_bytes), (2048, 1024));
        assert_eq!(
            config.cache_dir,
            PathBuf::from("/var/tmp/rampire-workload-run-1")
        );
        let mut cache = with("--mode", "file-cache");
        cache.extend(["--cache-dir".to_owned(), "/srv/cache".to_owned()]);
        let cache = parse(&cache).unwrap();
        assert_eq!(cache.mode, Mode::FileCache);
        assert_eq!(cache.cache_dir, PathBuf::from("/srv/cache"));
    }

    #[test]
    fn rejects_bounds_and_options_that_do_not_fit_together() {
        for bad in [
            with("--mode", "swap"),
            with("--peak-bytes", "8192"),
            with("--retained-bytes", "2048"),
            with("--peak-hold-seconds", "0"),
            with("--workload-id", "a b"),
            arguments(&["--cache-dir", "/tmp/x"]),
            arguments(&["--peak-bytes", "1"]),
            arguments(&["--surprise", "1"]),
            arguments(&["--cache-dir"]),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
        let mut relative = with("--mode", "file-cache");
        relative.extend(["--cache-dir".to_owned(), "cache".to_owned()]);
        assert!(parse(&relative).is_err());
    }

    #[test]
    fn file_cache_grows_and_shrinks_by_whole_files_and_cleans_up() {
        let directory =
            std::env::temp_dir().join(format!("rampire-workload-test-{}", std::process::id()));
        let mut cache = FileCache {
            directory: directory.clone(),
            files: Vec::new(),
        };
        cache.resize(FILE_BYTES + 1).unwrap();
        assert_eq!(cache.files.len(), 2);
        cache.resize(1).unwrap();
        assert_eq!(cache.files.len(), 1);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        drop(cache);
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn anonymous_memory_is_mapped_touched_and_released() {
        let mut load = Anonymous {
            regions: Vec::new(),
            touch: true,
        };
        load.resize(1 << 20).unwrap();
        load.refresh().unwrap();
        load.resize(1 << 16).unwrap();
        assert_eq!(load.regions.len(), 1);
    }
}
