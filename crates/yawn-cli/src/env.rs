use clap::ValueEnum;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// Full-screen interactive UI
    Tui,
    /// Line-oriented text, for CI, pipes, logs and terminals without TUI support
    Plain,
    /// One JSON object per line, for tools and agents
    Json,
}

#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub term: Option<String>,
    pub ci: bool,
    pub no_color: bool,
}

impl Caps {
    pub fn detect() -> Caps {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Caps {
            stdin_tty: std::io::stdin().is_terminal(),
            stdout_tty: std::io::stdout().is_terminal(),
            term: var("TERM"),
            ci: var("CI").is_some_and(|v| v != "0" && v != "false"),
            no_color: var("NO_COLOR").is_some(),
        }
    }

    pub fn dumb_terminal(&self) -> bool {
        self.term.as_deref().is_none_or(|t| t == "dumb")
    }
}

/// `--json` wins over `--plain`; otherwise the TUI is used only when both ends are a real, capable
/// terminal and we aren't running under CI
pub fn resolve_output(plain: bool, json: bool, caps: &Caps) -> Output {
    if json {
        Output::Json
    } else if plain || caps.ci || !caps.stdin_tty || !caps.stdout_tty || caps.dumb_terminal() {
        Output::Plain
    } else {
        Output::Tui
    }
}

pub fn color_enabled(choice: ColorChoice, caps: &Caps) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => !caps.no_color && caps.stdout_tty && !caps.dumb_terminal(),
    }
}

/// Facts about where Yawn runs, shown in the header and by `yawn info`
#[derive(Clone, Debug)]
pub struct EnvInfo {
    pub version: &'static str,
    pub cwd: String,
    /// Nearest directory with a `Cargo.toml`, relative to the cwd where possible
    pub cargo_project: Option<String>,
    pub term: String,
    pub color: bool,
}

impl EnvInfo {
    pub fn collect(caps: &Caps, color: bool) -> EnvInfo {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("?"));
        let project = find_up(&cwd, "Cargo.toml");
        EnvInfo {
            version: env!("CARGO_PKG_VERSION"),
            cwd: shorten_home(&cwd),
            cargo_project: project.map(|p| shorten_home(&p)),
            term: caps.term.clone().unwrap_or_else(|| "unknown".into()),
            color,
        }
    }
}

fn find_up(start: &Path, file: &str) -> Option<PathBuf> {
    start.ancestors().find(|d| d.join(file).is_file()).map(Path::to_path_buf)
}

fn shorten_home(p: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = p.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tty() -> Caps {
        Caps {
            stdin_tty: true,
            stdout_tty: true,
            term: Some("xterm-256color".into()),
            ci: false,
            no_color: false,
        }
    }

    #[test]
    fn tui_only_on_a_capable_terminal() {
        assert_eq!(resolve_output(false, false, &tty()), Output::Tui);
    }

    #[test]
    fn redirected_output_or_input_is_plain() {
        assert_eq!(
            resolve_output(false, false, &Caps { stdout_tty: false, ..tty() }),
            Output::Plain
        );
        assert_eq!(
            resolve_output(false, false, &Caps { stdin_tty: false, ..tty() }),
            Output::Plain
        );
    }

    #[test]
    fn ci_and_dumb_terminals_are_plain() {
        assert_eq!(resolve_output(false, false, &Caps { ci: true, ..tty() }), Output::Plain);
        assert_eq!(
            resolve_output(false, false, &Caps { term: Some("dumb".into()), ..tty() }),
            Output::Plain
        );
        assert_eq!(resolve_output(false, false, &Caps { term: None, ..tty() }), Output::Plain);
    }

    #[test]
    fn explicit_flags_win() {
        assert_eq!(resolve_output(true, false, &tty()), Output::Plain);
        assert_eq!(resolve_output(true, true, &tty()), Output::Json);
        assert_eq!(resolve_output(false, true, &Caps::default()), Output::Json);
    }

    #[test]
    fn color_rules() {
        assert!(color_enabled(ColorChoice::Auto, &tty()));
        assert!(!color_enabled(ColorChoice::Auto, &Caps { no_color: true, ..tty() }));
        assert!(!color_enabled(ColorChoice::Auto, &Caps { stdout_tty: false, ..tty() }));
        assert!(color_enabled(ColorChoice::Always, &Caps::default()));
        assert!(!color_enabled(ColorChoice::Never, &tty()));
    }
}
