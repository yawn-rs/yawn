use crate::env::EnvInfo;
use crate::event::{Event, Level, LogEvent};
use crate::pipeline::ReloadHandle;
use crate::state::State;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What the session is, for the panel
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub title: String,
    pub env: EnvInfo,
}

/// What the event loop has to do after a key
#[derive(Debug, PartialEq, Eq)]
pub enum Act {
    Nothing,
    Redraw,
    /// Wipe the screen and the scrollback
    Clear,
    /// The panel changed height
    Resize,
    Open(String),
}

pub struct App {
    pub state: State,
    pub info: SessionInfo,
    /// The command list under the panel is open
    pub more: bool,
    pub verbose: bool,
    pub trace: bool,
    pub quit: bool,
    pub rustc: Option<String>,
    /// What `r` does; none when the source has no reload
    pub reload: Option<ReloadHandle>,
    /// Log lines waiting to be printed above the panel
    pub pending: Vec<LogEvent>,
}

impl App {
    pub fn new(info: SessionInfo, level: Level) -> App {
        App {
            state: State::new(),
            info,
            verbose: level <= Level::Debug,
            trace: level <= Level::Trace,
            more: false,
            quit: false,
            rustc: None,
            reload: None,
            pending: Vec::new(),
        }
    }

    pub fn wants(&self, level: Level) -> bool {
        match level {
            Level::Debug => self.verbose,
            Level::Trace => self.trace,
            _ => true,
        }
    }

    pub fn on_event(&mut self, e: Event) {
        self.state.apply(&e);
        if let Event::Log(l) = e
            && self.wants(l.level)
        {
            self.pending.push(l);
        }
    }

    fn note(&mut self, message: String) {
        let at = self.state.now();
        self.pending.push(LogEvent::new(at, Level::Info, "yawn", message));
    }

    fn toggle(&mut self, name: &str, on: bool) {
        self.note(format!("{name} logs are now {}", if on { "on" } else { "off" }));
    }

    pub fn height(&self) -> u16 {
        if self.more { super::view::BIG } else { super::view::SMALL }
    }

    pub fn address(&self) -> Option<String> {
        let v = self.state.chip("server")?;
        if v.starts_with("http") {
            Some(v.to_string())
        } else if v.contains(':') && !v.contains(' ') {
            Some(format!("http://{v}"))
        } else {
            None
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Act {
        if key.kind == KeyEventKind::Release {
            return Act::Nothing;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.quit = true;
                Act::Redraw
            }
            KeyCode::Char('r') if !ctrl => {
                if let Some(r) = &self.reload {
                    r.trigger();
                }
                Act::Nothing
            }
            KeyCode::Char('o') if !ctrl => match self.address() {
                Some(url) => Act::Open(url),
                None => {
                    self.note("there is no server address to open yet".into());
                    Act::Redraw
                }
            },
            KeyCode::Char('v') if !ctrl => {
                self.verbose = !self.verbose;
                self.toggle("debug", self.verbose);
                Act::Redraw
            }
            KeyCode::Char('t') if !ctrl => {
                self.trace = !self.trace;
                self.toggle("trace", self.trace);
                Act::Redraw
            }
            KeyCode::Char('c') if !ctrl => Act::Clear,
            KeyCode::Char('/') => {
                self.more = !self.more;
                Act::Resize
            }
            _ => Act::Nothing,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventState;
    use std::time::Duration;

    pub fn app() -> App {
        let info = SessionInfo {
            title: "dev · demo".into(),
            env: EnvInfo {
                version: "0.0.0",
                cwd: "~/x".into(),
                cargo_project: None,
                term: "xterm".into(),
                color: false,
            },
        };
        App::new(info, Level::Info)
    }

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent { modifiers: KeyModifiers::CONTROL, ..key(KeyCode::Char(c)) }
    }

    fn info(value: &str) -> Event {
        Event::Info { at: Duration::ZERO, key: "server".into(), value: value.into() }
    }

    #[test]
    fn ctrl_c_quits_and_plain_q_does_not() {
        let mut a = app();
        assert_eq!(a.on_key(key(KeyCode::Char('q'))), Act::Nothing);
        assert!(!a.quit);
        a.on_key(ctrl('c'));
        assert!(a.quit);
    }

    #[test]
    fn debug_and_trace_logs_wait_for_their_toggle() {
        let mut a = app();
        let log = |level| Event::Log(LogEvent::new(Duration::ZERO, level, "s", "x"));
        a.on_event(log(Level::Info));
        a.on_event(log(Level::Debug));
        a.on_event(log(Level::Trace));
        assert_eq!(a.pending.len(), 1);
        a.on_key(key(KeyCode::Char('v')));
        a.on_event(log(Level::Debug));
        assert!(a.pending.iter().any(|l| l.message.contains("debug logs are now on")));
        assert_eq!(a.pending.iter().filter(|l| l.source == "s").count(), 2);
    }

    #[test]
    fn slash_toggles_the_command_list_and_the_panel_height() {
        let mut a = app();
        let small = a.height();
        assert_eq!(a.on_key(key(KeyCode::Char('/'))), Act::Resize);
        assert!(a.height() > small);
        a.on_key(key(KeyCode::Char('/')));
        assert_eq!(a.height(), small);
    }

    #[test]
    fn open_needs_a_server_address() {
        let mut a = app();
        assert_eq!(a.on_key(key(KeyCode::Char('o'))), Act::Redraw);
        assert!(a.pending[0].message.contains("no server address"));
        a.on_event(info("starting"));
        assert_eq!(a.address(), None);
        a.on_event(info("127.0.0.1:3000"));
        assert_eq!(a.on_key(key(KeyCode::Char('o'))), Act::Open("http://127.0.0.1:3000".into()));
        a.on_event(info("http://localhost:3000"));
        assert_eq!(a.address().as_deref(), Some("http://localhost:3000"));
    }

    #[test]
    fn clear_and_release_events() {
        let mut a = app();
        assert_eq!(a.on_key(key(KeyCode::Char('c'))), Act::Clear);
        let release = KeyEvent { kind: KeyEventKind::Release, ..key(KeyCode::Char('c')) };
        assert_eq!(a.on_key(release), Act::Nothing);
    }
}
