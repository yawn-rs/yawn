use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub const ALL: [Level; 5] =
        [Level::Trace, Level::Debug, Level::Info, Level::Warn, Level::Error];

    /// Fixed-width label for aligned columns
    pub fn label(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    /// Next minimum level when cycling through the filter, wrapping around
    pub fn cycle(self) -> Level {
        let i = Level::ALL.iter().position(|l| *l == self).unwrap_or(0);
        Level::ALL[(i + 1) % Level::ALL.len()]
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Level {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "trace" => Ok(Level::Trace),
            "debug" => Ok(Level::Debug),
            "info" => Ok(Level::Info),
            "warn" | "warning" => Ok(Level::Warn),
            "error" => Ok(Level::Error),
            _ => Err(format!("unknown level `{s}` (expected trace, debug, info, warn or error)")),
        }
    }
}

/// A position in source code, shown as `file:line:col`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

impl Location {
    pub fn new(file: impl AsRef<str>, line: u32, col: u32) -> Self {
        Location { file: clean(file.as_ref()), line, col }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.col)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEvent {
    /// Time since the session started
    pub at: Duration,
    pub level: Level,
    pub source: Cow<'static, str>,
    pub message: String,
    pub location: Option<Location>,
    /// Diagnostic code such as `E0308`
    pub code: Option<String>,
}

impl LogEvent {
    pub fn new(
        at: Duration,
        level: Level,
        source: impl Into<Cow<'static, str>>,
        message: impl AsRef<str>,
    ) -> Self {
        LogEvent {
            at,
            level,
            source: source.into(),
            message: clean(message.as_ref()),
            location: None,
            code: None,
        }
    }

    pub fn at_location(mut self, location: Location) -> Self {
        self.location = Some(location);
        self
    }

    pub fn with_code(mut self, code: impl AsRef<str>) -> Self {
        self.code = Some(clean(code.as_ref()));
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildEvent {
    Started {
        at: Duration,
        id: u32,
        target: String,
    },
    Progress {
        at: Duration,
        id: u32,
        done: u32,
        total: u32,
        label: String,
    },
    /// Progress of one of the parallel cargo builds (`client` or `server`)
    Lane {
        at: Duration,
        id: u32,
        lane: String,
        done: u32,
        total: u32,
    },
    Finished {
        at: Duration,
        id: u32,
        target: String,
        ok: bool,
        duration: Duration,
        warnings: u32,
        errors: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Log(LogEvent),
    Build(BuildEvent),
    /// A named piece of status ("server" = "http://localhost:3000") shown in the status panel
    Info {
        at: Duration,
        key: String,
        value: String,
    },
}

impl Event {
    pub fn at(&self) -> Duration {
        match self {
            Event::Log(l) => l.at,
            Event::Build(BuildEvent::Started { at, .. })
            | Event::Build(BuildEvent::Progress { at, .. })
            | Event::Build(BuildEvent::Lane { at, .. })
            | Event::Build(BuildEvent::Finished { at, .. })
            | Event::Info { at, .. } => *at,
        }
    }

    /// Text form used by the non-interactive outputs; build events become ordinary log lines
    pub fn to_log(&self) -> LogEvent {
        match self {
            Event::Log(l) => l.clone(),
            Event::Info { at, key, value } => {
                LogEvent::new(*at, Level::Trace, "status", format!("{key}: {value}"))
            }
            Event::Build(b) => match b {
                BuildEvent::Started { at, target, .. } => {
                    LogEvent::new(*at, Level::Info, "build", format!("started {target}"))
                }
                BuildEvent::Progress { at, done, total, label, .. } => {
                    LogEvent::new(*at, Level::Debug, "build", format!("[{done}/{total}] {label}"))
                }
                BuildEvent::Lane { at, lane, done, total, .. } => {
                    LogEvent::new(*at, Level::Trace, "build", format!("{lane} [{done}/{total}]"))
                }
                BuildEvent::Finished { at, target, ok, duration, warnings, errors, .. } => {
                    let verdict = if *ok { "finished" } else { "failed" };
                    let plural = |n: &u32| if *n == 1 { "" } else { "s" };
                    LogEvent::new(
                        *at,
                        if *ok { Level::Info } else { Level::Error },
                        "build",
                        format!(
                            "{verdict} {target} in {} ({errors} error{}, {warnings} warning{})",
                            fmt_duration(*duration),
                            plural(errors),
                            plural(warnings)
                        ),
                    )
                }
            },
        }
    }
}

/// Makes arbitrary text safe to print: drops control characters and ANSI escape sequences (which a
/// child process may emit and which could otherwise move the cursor or corrupt the screen) and
/// flattens line breaks
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for n in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&n) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        // OSC: ends at BEL or ESC \
                        while let Some(n) = chars.next() {
                            if n == '\x07' {
                                break;
                            }
                            if n == '\x1b' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\n' => out.push('↵'),
            '\r' => {}
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", d.as_secs_f64())
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

/// `12345` -> `12,345`
pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_escapes_and_controls() {
        assert_eq!(clean("\x1b[31mred\x1b[0m ok"), "red ok");
        assert_eq!(clean("a\x1b]0;title\x07b"), "ab");
        assert_eq!(clean("a\nb\r\tc"), "a↵b    c");
        assert_eq!(clean("bell\x07here"), "bellhere");
        assert_eq!(clean("zażółć 日本"), "zażółć 日本");
    }

    #[test]
    fn level_parse_and_cycle() {
        assert_eq!("WARN".parse::<Level>().unwrap(), Level::Warn);
        assert!("loud".parse::<Level>().is_err());
        assert_eq!(Level::Error.cycle(), Level::Trace);
        assert!(Level::Warn > Level::Info);
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(fmt_duration(Duration::from_millis(1840)), "1.84s");
        assert_eq!(fmt_duration(Duration::from_secs(65)), "1m05s");
    }

    #[test]
    fn build_events_describe_as_log_lines() {
        let e = Event::Build(BuildEvent::Finished {
            at: Duration::ZERO,
            id: 1,
            target: "app".into(),
            ok: false,
            duration: Duration::from_millis(900),
            warnings: 2,
            errors: 1,
        });
        let l = e.to_log();
        assert_eq!(l.level, Level::Error);
        assert!(l.message.contains("failed app in 900ms"));
    }
}
