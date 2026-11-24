//! Whether i915 starts an Intel GPU with its GuC (the microcontroller that
//! submits its work), and the record a boot leaves for the next one to
//! decide it by.
//!
//! On the platforms whose i915 default is GuC submission (Alder Lake P,
//! Raptor Lake P and U, Alder Lake N, Raptor Lake S, DG1) a failed GuC
//! wedges the GT or loses the Metal device, and i915 cannot be unbound and
//! probed again in the same boot: its unload is never run here, and a
//! failed probe's interrupt line is retired for good. So the boot that sees
//! the GuC fail writes a [`RECORD`] line in its log, and the next boot,
//! reading it back as the Wi-Fi page's record is read
//! (`stela-boot`'s `log_file`, handed to the engine with the rest of the
//! loader's reading: the engine runs a copy of the image taken before any
//! static was set), starts i915 with `i915.enable_guc=0`: its
//! execlists submission, which every Gen12 part has, with i915's own
//! frequency and RC6 control in place of the GuC's SLPC. A boot without the
//! GuC writes the record again, so the boots after it stay without it.
//!
//! A record names the device and this build ([`BUILD`]); one of another
//! device (the stick moved to another PC) or another build (a newer fix to
//! try) is passed over and the GuC is tried again. `--i915-guc off` turns
//! the GuC off by hand and `--i915-guc on` asks for it, the record still
//! followed; `--i915-guc auto`, the default, is off on Alder Lake P and
//! Raptor Lake P or U in this test build ([`off_by_default`]) and follows
//! the record elsewhere.
use alloc::{format, string::String};

/// What each record line starts with, after the log's time stamp.
pub const RECORD: &str = "INTEL GUC RECORD ";
/// The build whose records this one follows.
pub const BUILD: &str = "dev64-2";
/// The marker line each boot's log starts with.
const BOOT: &str = "=== STELA BOOT ";

/// The hand-written `--i915-guc` setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Setting {
    /// The platform's default ([`off_by_default`]); elsewhere the GuC,
    /// unless the earlier boot recorded it failing.
    #[default]
    Auto,
    /// No GuC: execlists submission.
    Off,
    /// The GuC, unless the earlier boot recorded it failing, also where
    /// this build starts without it by default.
    On,
}

impl Setting {
    const NAMES: [(&'static str, Self); 3] =
        [("auto", Self::Auto), ("off", Self::Off), ("on", Self::On)];

    pub fn parse(word: &str) -> Option<Self> {
        Self::NAMES
            .iter()
            .find(|(name, _)| *name == word)
            .map(|(_, setting)| *setting)
    }

    pub fn name(self) -> &'static str {
        Self::NAMES
            .iter()
            .find(|(_, setting)| *setting == self)
            .map(|(name, _)| *name)
            .unwrap_or_default()
    }
}

/// Whether i915's GuC submission is its default on the platform i915's
/// table names (`uc_expand_default_options`), and execlists still drive it
/// with `enable_guc=0`. Tiger and Rocket Lake and Alder Lake S load no GuC
/// for submission by default; DG2 and Meteor Lake are left as they are.
pub fn falls_back(platform: &str) -> bool {
    matches!(platform, "ALDERLAKE_P" | "ALDERLAKE_S" | "DG1")
}

/// Whether this test build starts the platform without its GuC unless
/// `--i915-guc on` asks for it: Alder Lake P, whose table holds Raptor Lake
/// P and U (dev64's a7a0, Ashu's a7ab) and Alder Lake N. Their GuC started
/// in dev64-1, and each of Mesa's submissions still failed, so this build
/// tests execlists in one boot.
pub fn off_by_default(platform: &str) -> bool {
    platform == "ALDERLAKE_P"
}

/// How a boot's GuC failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// i915's GuC start failed with this Linux error (`__uc_init_hw`'s
    /// "GuC initialization failed").
    Init(i32),
    /// The GT was declared wedged at its start.
    Wedged,
    /// An error of the GuC's command transport (`GUC: CT:`).
    Transport,
    /// Another error i915 printed of its GuC.
    Error,
    /// The GPU's Metal device did not open while the GuC ran.
    Metal,
}

impl Cause {
    fn word(self) -> String {
        match self {
            Self::Init(error) => format!("init{error}"),
            Self::Wedged => String::from("wedged"),
            Self::Transport => String::from("transport"),
            Self::Error => String::from("error"),
            Self::Metal => String::from("metal"),
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "wedged" => Some(Self::Wedged),
            "transport" => Some(Self::Transport),
            "error" => Some(Self::Error),
            "metal" => Some(Self::Metal),
            _ => word.strip_prefix("init")?.parse().ok().map(Self::Init),
        }
    }

    /// The cause in the words of the boot's mode line.
    pub fn told(self) -> String {
        match self {
            Self::Init(error) => format!("its start failed ({error})"),
            Self::Wedged => String::from("the GT was declared wedged"),
            Self::Transport => String::from("its command transport failed"),
            Self::Error => String::from("i915 reported a GuC error"),
            Self::Metal => String::from("the Metal device did not open"),
        }
    }

    /// The cause a line i915 printed at error level or above tells of its
    /// GuC, if any.
    pub fn of_line(line: &str) -> Option<Self> {
        if let Some((_, rest)) = line.split_once("GuC initialization failed ") {
            let error = rest.split_ascii_whitespace().next()?;
            return Some(Self::Init(error.parse().unwrap_or(0)));
        }
        if line.contains("Failed to initialize GPU, declaring it wedged") {
            return Some(Self::Wedged);
        }
        if line.contains("GUC: CT:") {
            return Some(Self::Transport);
        }
        (line.contains("GuC") || line.contains("GUC:")).then_some(Self::Error)
    }
}

/// One record line: the GuC of `device` failed this boot (`kept` false),
/// or this boot ran without it after a failure and keeps it off (`kept`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub device: u16,
    pub cause: Cause,
    pub kept: bool,
}

impl Record {
    /// The log line, without the time stamp the log adds.
    pub fn line(self) -> String {
        format!(
            "{RECORD}{} device={:04x} cause={} build={BUILD}\n",
            if self.kept { "kept-off" } else { "failed" },
            self.device,
            self.cause.word()
        )
    }

    /// The record of one log line of this build. Only a line that is the
    /// record itself counts, not one that quotes it (a `PREVIOUS BOOT`
    /// line repeats an earlier boot's).
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim_start();
        let line = match line.strip_prefix('[') {
            Some(stamped) => stamped.split_once("] ")?.1,
            None => line,
        };
        let mut words = line.strip_prefix(RECORD)?.split_ascii_whitespace();
        let kept = match words.next()? {
            "failed" => false,
            "kept-off" => true,
            _ => return None,
        };
        let device = u16::from_str_radix(words.next()?.strip_prefix("device=")?, 16).ok()?;
        let cause = Cause::parse(words.next()?.strip_prefix("cause=")?)?;
        (words.next()?.strip_prefix("build=")? == BUILD).then_some(Self {
            device,
            cause,
            kept,
        })
    }
}

/// The newest record of the earlier boot in `stream`, the log's newest
/// bytes, read before this boot's marker line joins them.
pub fn earlier(stream: &[u8]) -> Option<Record> {
    let text = String::from_utf8_lossy(stream);
    let boot = text.rfind(BOOT).map_or(&text[..], |at| &text[at..]);
    boot.lines().rev().find_map(Record::parse)
}

/// Whether this boot starts i915's GuC, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// GuC submission: i915's default, or `--i915-guc on`.
    Guc,
    /// No GuC: `--i915-guc off`.
    OffByHand,
    /// No GuC: this build's default on the platform ([`off_by_default`]).
    OffByDefault,
    /// No GuC: the earlier boot's record of this device.
    Fallback(Cause),
}

impl Mode {
    /// The mode for `device`, of i915's `platform`, under `setting` and the
    /// earlier boot's `record`.
    pub fn decide(setting: Setting, record: Option<Record>, device: u16, platform: &str) -> Self {
        match (setting, record) {
            (Setting::Off, _) => Self::OffByHand,
            (Setting::Auto, _) if off_by_default(platform) => Self::OffByDefault,
            (Setting::Auto | Setting::On, Some(record)) if record.device == device => {
                Self::Fallback(record.cause)
            }
            (Setting::Auto | Setting::On, _) => Self::Guc,
        }
    }

    pub fn guc(self) -> bool {
        self == Self::Guc
    }

    /// The boot's one line saying which mode ran and why.
    pub fn line(self, device: u16, record: Option<Record>) -> String {
        match self {
            Self::Guc => {
                let passed = match record {
                    Some(record) if record.device != device => format!(
                        "; the record of device {:04x} is another PC's",
                        record.device
                    ),
                    _ => String::new(),
                };
                format!(
                    "INTEL GUC on ({BUILD}): i915 starts {device:04x} with GuC submission and \
                     its guc_id taken before the lock{passed}; a GuC failure makes the next \
                     start go without it\n"
                )
            }
            Self::OffByHand => format!(
                "INTEL GUC off ({BUILD}): --i915-guc off, starting i915 on {device:04x} without \
                 GuC (execlists)\n"
            ),
            Self::OffByDefault => format!(
                "INTEL GUC off ({BUILD}): this test build's default on Alder Lake P and Raptor \
                 Lake P or U, starting i915 on {device:04x} without GuC (execlists); \
                 --i915-guc on starts it with GuC submission\n"
            ),
            Self::Fallback(cause) => format!(
                "INTEL GUC fallback ({BUILD}): GuC failed last boot ({}), starting i915 on \
                 {device:04x} without GuC (execlists)\n",
                cause.told()
            ),
        }
    }
}

/// The stop a boot whose GuC failed and that has no Metal device gives.
pub fn second_start(cause: Cause) -> String {
    format!(
        "Intel graphics needs a second start; restart the PC. The GuC failed ({}), and the \
         next start runs i915 without it",
        cause.told()
    )
}

#[cfg(test)]
mod tests;
