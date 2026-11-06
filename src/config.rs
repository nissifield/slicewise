//! Configuration file model (design §7): serde structs, defaults, validation,
//! `parse_size` and path resolution. Unknown keys are errors.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

const NAME: &str = env!("CARGO_PKG_NAME");
const GIB: u64 = 1 << 30;
const UNIT_SUFFIXES: [&str; 3] = [".service", ".slice", ".scope"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Size(pub u64);

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SizeVisitor;
        impl Visitor<'_> for SizeVisitor {
            type Value = Size;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte count or a size string with a K/M/G/T suffix")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Size, E> {
                Ok(Size(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Size, E> {
                u64::try_from(value)
                    .map(Size)
                    .map_err(|_| E::custom("size must not be negative"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Size, E> {
                parse_size(value).map(Size).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(SizeVisitor)
    }
}

/// Plain bytes or a `K/M/G/T` binary suffix.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (digits, scale) = match text.bytes().last() {
        Some(b'K') => (&text[..text.len() - 1], 1 << 10),
        Some(b'M') => (&text[..text.len() - 1], 1 << 20),
        Some(b'G') => (&text[..text.len() - 1], 1 << 30),
        Some(b'T') => (&text[..text.len() - 1], 1 << 40),
        Some(b) if b.is_ascii_digit() => (text, 1),
        _ => return Err(invalid_size(text)),
    };
    let value: u64 = digits.parse().map_err(|_| invalid_size(text))?;
    value
        .checked_mul(scale)
        .ok_or_else(|| format!("size {text:?} overflows u64"))
}

fn invalid_size(text: &str) -> String {
    format!("invalid size {text:?}: expected bytes or a K/M/G/T suffix")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Interactive,
    Normal,
    Batch,
}

impl Class {
    pub const ALL: [Class; 3] = [Class::Interactive, Class::Normal, Class::Batch];

    pub fn index(self) -> usize {
        match self {
            Class::Interactive => 0,
            Class::Normal => 1,
            Class::Batch => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Interactive => "interactive",
            Class::Normal => "normal",
            Class::Batch => "batch",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ClassDefaults {
    cpu_weight: u32,
    default_mem: Size,
    oom_score_adj: Option<i32>,
}

const INTERACTIVE: ClassDefaults = ClassDefaults {
    cpu_weight: 400,
    default_mem: Size(2 * GIB),
    oom_score_adj: None,
};

const NORMAL: ClassDefaults = ClassDefaults {
    cpu_weight: 100,
    default_mem: Size(2 * GIB),
    oom_score_adj: Some(300),
};

const BATCH: ClassDefaults = ClassDefaults {
    cpu_weight: 25,
    default_mem: Size(2 * GIB),
    oom_score_adj: Some(800),
};

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClassConfig {
    pub cpu_weight: Option<u32>,
    pub default_mem: Option<Size>,
    pub oom_score_adj: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Classes {
    pub interactive: ClassConfig,
    pub normal: ClassConfig,
    pub batch: ClassConfig,
}

impl Classes {
    fn defaults(class: Class) -> ClassDefaults {
        match class {
            Class::Interactive => INTERACTIVE,
            Class::Normal => NORMAL,
            Class::Batch => BATCH,
        }
    }

    pub fn get(&self, class: Class) -> &ClassConfig {
        match class {
            Class::Interactive => &self.interactive,
            Class::Normal => &self.normal,
            Class::Batch => &self.batch,
        }
    }

    pub fn cpu_weight(&self, class: Class) -> u32 {
        self.get(class)
            .cpu_weight
            .unwrap_or(Self::defaults(class).cpu_weight)
    }

    pub fn default_mem(&self, class: Class) -> Size {
        self.get(class)
            .default_mem
            .unwrap_or(Self::defaults(class).default_mem)
    }

    pub fn oom_score_adj(&self, class: Class) -> Option<i32> {
        self.get(class)
            .oom_score_adj
            .or(Self::defaults(class).oom_score_adj)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Protected {
    pub unit: String,
    #[serde(default = "default_protected_weight")]
    pub cpu_weight: u32,
    #[serde(default = "default_protected_memory_low")]
    pub memory_low: Size,
}

fn default_protected_weight() -> u32 {
    200
}

fn default_protected_memory_low() -> Size {
    Size(2 * GIB)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tuning {
    pub cpu_psi_engage: f32,
    pub cpu_psi_release: f32,
    pub min_write_interval_secs: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            cpu_psi_engage: 20.0,
            cpu_psi_release: 5.0,
            min_write_interval_secs: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub tick_secs: u64,
    pub pool_slice: String,
    pub pool_memory_max: Size,
    pub system_reserve: Size,
    pub reserve_cpus: u32,
    pub mem_per_job: Size,
    pub jobs_env: Vec<String>,
    pub protected: Vec<Protected>,
    #[serde(rename = "class")]
    pub classes: Classes,
    pub tuning: Tuning,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tick_secs: 2,
            pool_slice: default_pool_slice(),
            pool_memory_max: Size(0),
            system_reserve: Size(2 * GIB),
            reserve_cpus: 1,
            mem_per_job: Size(GIB),
            jobs_env: vec![
                "SLICEWISE_JOBS".to_string(),
                "CARGO_BUILD_JOBS".to_string(),
                "CMAKE_BUILD_PARALLEL_LEVEL".to_string(),
            ],
            protected: Vec::new(),
            classes: Classes::default(),
            tuning: Tuning::default(),
        }
    }
}

fn default_pool_slice() -> String {
    format!("app-{NAME}.slice")
}

impl Config {
    /// Class slice derived from the pool stem (design §5.2).
    pub fn class_slice(&self, class: Class) -> String {
        let stem = self
            .pool_slice
            .strip_suffix(".slice")
            .unwrap_or(&self.pool_slice);
        format!("{stem}-{}.slice", class.as_str())
    }

    /// Parse a config without touching the filesystem; validation is separate.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Read, parse and resolve; `mem_total` is injected for testability (§7).
    pub fn load(path: &Path, mem_total: u64) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("config error: cannot read {}: {e}", path.display()))?;
        let config =
            Self::parse(&text).map_err(|e| format!("config error in {}: {e}", path.display()))?;
        config.resolve(mem_total)
    }

    /// Fill an auto (`0`) pool cap, then validate (design §7).
    pub fn resolve(mut self, mem_total: u64) -> Result<Self, String> {
        if self.pool_memory_max.0 == 0 {
            self.pool_memory_max = Size(mem_total.saturating_sub(self.system_reserve.0) / 4 * 3);
        }
        self.validate(mem_total)?;
        Ok(self)
    }

    pub fn validate(&self, mem_total: u64) -> Result<(), String> {
        // design: §7 — the tick drives tick-count hysteresis, so cap it at 10 s.
        if !(1..=10).contains(&self.tick_secs) {
            return invalid(format!("tick_secs = {}: must be 1..=10", self.tick_secs));
        }
        if self.tuning.min_write_interval_secs < 10 {
            return invalid(format!(
                "tuning.min_write_interval_secs = {}: must be >= 10",
                self.tuning.min_write_interval_secs
            ));
        }
        let (engage, release) = (self.tuning.cpu_psi_engage, self.tuning.cpu_psi_release);
        if !(engage.is_finite()
            && release.is_finite()
            && 0.0 < release
            && release < engage
            && engage <= 100.0)
        {
            return invalid(format!(
                "tuning.cpu_psi_engage = {engage}, cpu_psi_release = {release}: must be finite with 0 < release < engage <= 100"
            ));
        }
        if !self.pool_slice.ends_with(".slice") || !valid_unit_name(&self.pool_slice) {
            return invalid(format!(
                "pool_slice = {:?}: must be a valid .slice unit name",
                self.pool_slice
            ));
        }
        check_size("pool_memory_max", self.pool_memory_max.0, mem_total)?;
        check_size("system_reserve", self.system_reserve.0, mem_total)?;
        check_size("mem_per_job", self.mem_per_job.0, mem_total)?;
        if self.mem_per_job.0 == 0 {
            return invalid("mem_per_job = 0: must be > 0");
        }
        let frame = self.pool_memory_max.0.saturating_add(self.system_reserve.0);
        if frame > mem_total {
            return invalid(format!(
                "pool_memory_max + system_reserve = {frame}: must be <= MemTotal {mem_total}"
            ));
        }
        for class in Class::ALL {
            check_weight(
                &format!("class.{}.cpu_weight", class.as_str()),
                self.classes.cpu_weight(class),
            )?;
            check_size(
                &format!("class.{}.default_mem", class.as_str()),
                self.classes.default_mem(class).0,
                mem_total,
            )?;
            if let Some(adj) = self.classes.oom_score_adj(class)
                && !(-1000..=1000).contains(&adj)
            {
                return invalid(format!(
                    "class.{}.oom_score_adj = {adj}: must be -1000..=1000",
                    class.as_str()
                ));
            }
        }
        for (index, protected) in self.protected.iter().enumerate() {
            if !valid_unit_name(&protected.unit) {
                return invalid(format!(
                    "protected[{index}].unit = {:?}: invalid systemd unit name",
                    protected.unit
                ));
            }
            check_weight(
                &format!("protected[{index}].cpu_weight"),
                protected.cpu_weight,
            )?;
            check_size(
                &format!("protected[{index}].memory_low"),
                protected.memory_low.0,
                mem_total,
            )?;
        }
        Ok(())
    }
}

fn check_weight(field: &str, weight: u32) -> Result<(), String> {
    if (1..=10000).contains(&weight) {
        Ok(())
    } else {
        invalid(format!("{field} = {weight}: must be 1..=10000"))
    }
}

fn check_size(field: &str, size: u64, mem_total: u64) -> Result<(), String> {
    if size <= mem_total {
        Ok(())
    } else {
        invalid(format!("{field} = {size}: must be <= MemTotal {mem_total}"))
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T, String> {
    Err(format!("config error: {}", message.into()))
}

/// Systemd unit name character set and a known suffix (design §9).
pub fn valid_unit_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'\\' | b'@' | b'-')
        })
        && UNIT_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Resolve `$XDG_CONFIG_HOME/slicewise/config.toml`, else `$HOME/.config/...`.
pub fn config_path(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        return Some(xdg.join(NAME).join("config.toml"));
    }
    home.map(|home| home.join(".config").join(NAME).join("config.toml"))
}

/// The same resolution from the process environment; no `/tmp` fallback.
pub fn discover_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    config_path(xdg.as_deref(), home.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_bytes_and_suffixes() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("512"), Ok(512));
        assert_eq!(parse_size("512M"), Ok(512 * (1 << 20)));
        assert_eq!(parse_size("6G"), Ok(6 << 30));
        assert_eq!(parse_size("1T"), Ok(1 << 40));
        assert_eq!(parse_size("2K"), Ok(2048));
    }

    #[test]
    fn parse_size_rejects_bad_input() {
        assert!(parse_size("").is_err());
        assert!(parse_size("12X").is_err());
        assert!(parse_size("M").is_err());
        assert!(parse_size("-1").is_err());
        assert!(parse_size("infinity").is_err());
        assert!(parse_size("18446744073709551615T").is_err());
    }

    #[test]
    fn defaults_match_design_table() {
        let config = Config::default();
        assert_eq!(config.tick_secs, 2);
        assert_eq!(config.pool_slice, "app-slicewise.slice");
        assert_eq!(config.pool_memory_max, Size(0));
        assert_eq!(config.system_reserve, Size(2 << 30));
        assert_eq!(config.reserve_cpus, 1);
        assert_eq!(config.mem_per_job, Size(1 << 30));
        assert_eq!(config.classes.cpu_weight(Class::Interactive), 400);
        assert_eq!(config.classes.oom_score_adj(Class::Normal), Some(300));
        assert_eq!(config.classes.oom_score_adj(Class::Batch), Some(800));
        assert_eq!(
            config.class_slice(Class::Batch),
            "app-slicewise-batch.slice"
        );
    }

    #[test]
    fn pool_default_resolves_relative_to_mem_total() {
        let small = Config::default().resolve(4 << 30).unwrap();
        assert_eq!(small.pool_memory_max, Size(3 << 29));
        let large = Config::default().resolve(64 << 30).unwrap();
        assert_eq!(large.pool_memory_max, Size(93 << 29));
    }

    #[test]
    fn unknown_keys_are_errors() {
        let error = Config::parse("mystery = 1").unwrap_err().to_string();
        assert!(error.contains("unknown field `mystery`"), "{error}");
        let error = Config::parse("[class.batch]\nmystery = 1\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `mystery`"), "{error}");
    }

    #[test]
    fn sizes_accept_integers_and_strings_and_partial_classes_keep_defaults() {
        let config =
            Config::parse("pool_memory_max = 1073741824\n[class.batch]\ndefault_mem = \"512M\"\n")
                .unwrap();
        assert_eq!(config.pool_memory_max, Size(1 << 30));
        assert_eq!(config.classes.default_mem(Class::Batch), Size(512 << 20));
        assert_eq!(config.classes.cpu_weight(Class::Batch), 25);
        assert_eq!(config.classes.oom_score_adj(Class::Batch), Some(800));
    }

    #[test]
    fn validation_reports_precise_errors() {
        let mut config = Config::default();
        config.classes.batch.cpu_weight = Some(0);
        assert_eq!(
            config.validate(64 << 30).unwrap_err(),
            "config error: class.batch.cpu_weight = 0: must be 1..=10000"
        );

        let config = Config {
            pool_memory_max: Size(64 << 30),
            ..Config::default()
        };
        assert_eq!(
            config.validate(16 << 30).unwrap_err(),
            "config error: pool_memory_max = 68719476736: must be <= MemTotal 17179869184"
        );

        let config = Config {
            pool_memory_max: Size(15 << 30),
            system_reserve: Size(2 << 30),
            ..Config::default()
        };
        assert_eq!(
            config.validate(16 << 30).unwrap_err(),
            "config error: pool_memory_max + system_reserve = 18253611008: must be <= MemTotal 17179869184"
        );

        let config = Config {
            mem_per_job: Size(0),
            ..Config::default()
        };
        assert_eq!(
            config.validate(16 << 30).unwrap_err(),
            "config error: mem_per_job = 0: must be > 0"
        );

        let mut config = Config::default();
        config.protected.push(Protected {
            unit: "not a unit".into(),
            cpu_weight: 200,
            memory_low: Size(0),
        });
        assert!(
            config
                .validate(16 << 30)
                .unwrap_err()
                .contains("protected[0].unit = \"not a unit\": invalid systemd unit name")
        );
    }

    #[test]
    fn tick_secs_must_be_in_1_to_10() {
        for tick in [0, 11] {
            let config = Config {
                tick_secs: tick,
                ..Config::default()
            };
            assert!(
                config
                    .validate(64 << 30)
                    .unwrap_err()
                    .contains("tick_secs = ")
            );
        }
    }

    #[test]
    fn min_write_interval_is_at_least_ten() {
        let config = Config {
            tuning: Tuning {
                min_write_interval_secs: 9,
                ..Tuning::default()
            },
            ..Config::default()
        };
        assert!(
            config
                .validate(64 << 30)
                .unwrap_err()
                .contains("min_write_interval_secs = 9: must be >= 10")
        );
    }

    #[test]
    fn psi_thresholds_are_finite_and_ordered() {
        for tuning in [
            Tuning {
                cpu_psi_engage: f32::NAN,
                ..Tuning::default()
            },
            Tuning {
                cpu_psi_engage: 20.0,
                cpu_psi_release: 20.0,
                ..Tuning::default()
            },
            Tuning {
                cpu_psi_engage: 101.0,
                ..Tuning::default()
            },
        ] {
            let config = Config {
                tuning,
                ..Config::default()
            };
            assert!(config.validate(64 << 30).unwrap_err().contains("cpu_psi"));
        }
    }

    #[test]
    fn oom_score_adj_is_range_checked() {
        let mut config = Config::default();
        config.classes.batch.oom_score_adj = Some(1001);
        assert!(
            config
                .validate(64 << 30)
                .unwrap_err()
                .contains("class.batch.oom_score_adj = 1001: must be -1000..=1000")
        );
    }

    #[test]
    fn valid_units_need_a_known_suffix() {
        assert!(valid_unit_name("orchestrator.service"));
        assert!(valid_unit_name("app-slicewise-batch.slice"));
        assert!(!valid_unit_name("no-suffix"));
        assert!(!valid_unit_name("bad name.service"));
        assert!(!valid_unit_name(""));
    }

    #[test]
    fn config_path_prefers_xdg_then_home() {
        let xdg = Path::new("<xdg>");
        let home = Path::new("<home>");
        assert_eq!(
            config_path(Some(xdg), Some(home)),
            Some(PathBuf::from("<xdg>/slicewise/config.toml"))
        );
        assert_eq!(
            config_path(None, Some(home)),
            Some(PathBuf::from("<home>/.config/slicewise/config.toml"))
        );
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn parse_error_names_the_file() {
        let dir = std::env::temp_dir().join(format!("slicewise-config-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "pool_memory_max = 12G\n").unwrap();
        let error = Config::load(&path, 64 << 30).unwrap_err().to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(error.contains("config error in"), "{error}");
        assert!(error.contains("TOML parse error"), "{error}");
    }
}
