use serde::Deserialize;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "Yawn.toml";
pub const DEFAULT_PORT: u16 = 3000;
/// Where `yawn dev` writes the compiled client, relative to the project root
pub const DIST_DIR: &str = "target/yawn/dist";
/// What a `yawn dev` session keeps for itself: the literals it patches in without a build
pub const DEV_DIR: &str = "target/yawn/dev";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    pub app: AppSection,
    pub server: ServerSection,
    pub dev: DevSection,
    pub build: BuildSection,
    pub router: RouterSection,
    pub tailwind: TailwindSection,
}

/// `[tailwind]`: build Tailwind CSS from the project's `.rsx` files and link it in the page
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TailwindSection {
    /// Turns the integration on (class completion in the editor, CSS built by `yawn dev`)
    pub enabled: bool,
    /// Stylesheet with your `@import "tailwindcss"`, `@theme` and `@source` rules, relative to the
    /// project root
    pub input: Option<String>,
    /// Path of a `tailwindcss` executable to use
    pub binary: Option<String>,
}

/// Where `yawn dev` writes the Tailwind output, under the dist directory; served at
/// [`TAILWIND_URL`]
pub const TAILWIND_FILE: &str = "pkg/tailwind.css";
pub const TAILWIND_URL: &str = "/pkg/tailwind.css";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterSection {
    /// Turn `src/routes/` into routes (Next.js style)
    pub folders: bool,
}

impl Default for RouterSection {
    fn default() -> Self {
        RouterSection { folders: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppSection {
    /// Display name; defaults to the Cargo package name
    pub name: Option<String>,
    /// HTML `<title>`
    pub title: Option<String>,
    /// Stylesheet URLs linked from the shell
    pub stylesheets: Option<Vec<String>>,
    /// Script URLs added at the end of the page body, in this order
    pub scripts: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSection {
    pub host: Option<IpAddr>,
    pub port: Option<u16>,
}

/// `[build]`: what `yawn build` puts next to the server in `dist/`
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildSection {
    /// Directories copied into `dist/` under their own name, relative to the project root (they may
    /// lie outside it, `../translations`)
    pub copy: Vec<String>,
    /// Pages `yawn build --static` renders besides the ones in `src/routes/` without parameters:
    /// `["/users/1", "/docs/intro"]`
    pub prerender: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DevSection {
    /// Directory served at `/assets`, relative to the project root
    pub assets: Option<String>,
    /// Extra directories `yawn dev` watches, relative to the project root (they may lie outside it,
    /// `../translations`)
    pub watch: Vec<String>,
    /// Splits the project's modules into several crates while developing, so a change recompiles
    /// only the crate it's in and what uses it (default: on; `yawn build` always builds one crate,
    /// which is a little smaller)
    pub split: Option<bool>,
    /// Builds the app position-independent and a route edit as a small module the open page loads
    /// instead of rebuilding the app (default: on; needs the `rust-src` component, without it the
    /// app is rebuilt as usual)
    pub dynamic: Option<bool>,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, String),
    BadPort { source: &'static str, value: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(p, e) => write!(f, "cannot read {}: {e}", p.display()),
            ConfigError::Parse(p, e) => write!(f, "invalid {}: {e}", p.display()),
            ConfigError::BadPort { source, value } => {
                write!(f, "invalid port `{value}` from {source} (expected 0-65535)")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl ProjectConfig {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Loads `<dir>/Yawn.toml`; `Ok(None)` when the file doesn't exist
    pub fn load(dir: &Path) -> Result<Option<Self>, ConfigError> {
        let path = dir.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map(Some).map_err(|e| ConfigError::Parse(path, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ConfigError::Io(path, e)),
        }
    }

    pub fn assets_dir(&self) -> &str {
        self.dev.assets.as_deref().unwrap_or("assets")
    }
}

/// Nearest ancestor of `start` (inclusive) that contains `Yawn.toml`
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|d| d.join(CONFIG_FILE).is_file()).map(Path::to_path_buf)
}

/// `[package] name` of `<dir>/Cargo.toml`
pub fn read_package_name(dir: &Path) -> Result<String, ConfigError> {
    let path = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| ConfigError::Io(path.clone(), e))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| ConfigError::Parse(path.clone(), e.to_string()))?;
    table
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .ok_or_else(|| ConfigError::Parse(path, "missing [package] name".into()))
}

/// Version of `package` pinned in `<dir>/Cargo.lock`, if the lockfile exists and lists it
pub fn locked_version(dir: &Path, package: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("Cargo.lock")).ok()?;
    let table: toml::Table = text.parse().ok()?;
    table.get("package")?.as_array()?.iter().find_map(|p| {
        (p.get("name")?.as_str()? == package)
            .then(|| p.get("version")?.as_str().map(str::to_string))?
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSource {
    /// Explicit command-line argument (or the launcher acting for it)
    Cli,
    /// The `PORT` environment variable
    Env,
    /// `[server] port` in `Yawn.toml`
    Config,
    Default,
}

impl PortSource {
    pub fn describe(self) -> &'static str {
        match self {
            PortSource::Cli => "--port",
            PortSource::Env => "PORT",
            PortSource::Config => "Yawn.toml",
            PortSource::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPort {
    pub port: u16,
    pub source: PortSource,
}

/// Priority: CLI argument, `PORT` variable, project configuration, default (3000)
pub fn resolve_port(
    cli: Option<u16>,
    env: Option<&str>,
    config: Option<u16>,
) -> Result<ResolvedPort, ConfigError> {
    if let Some(port) = cli {
        return Ok(ResolvedPort { port, source: PortSource::Cli });
    }
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        let port =
            v.parse().map_err(|_| ConfigError::BadPort { source: "PORT", value: v.to_string() })?;
        return Ok(ResolvedPort { port, source: PortSource::Env });
    }
    if let Some(port) = config {
        return Ok(ResolvedPort { port, source: PortSource::Config });
    }
    Ok(ResolvedPort { port: DEFAULT_PORT, source: PortSource::Default })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_priority_cli_env_config_default() {
        let r = |c, e, f| resolve_port(c, e, f).unwrap();
        assert_eq!(
            r(Some(1), Some("2"), Some(3)),
            ResolvedPort { port: 1, source: PortSource::Cli }
        );
        assert_eq!(r(None, Some("2"), Some(3)), ResolvedPort { port: 2, source: PortSource::Env });
        assert_eq!(r(None, None, Some(3)), ResolvedPort { port: 3, source: PortSource::Config });
        assert_eq!(r(None, None, None), ResolvedPort { port: 3000, source: PortSource::Default });
        assert_eq!(r(None, Some("  "), None).source, PortSource::Default, "blank PORT is unset");
    }

    #[test]
    fn invalid_env_port_is_an_error() {
        assert!(resolve_port(None, Some("abc"), None).is_err());
        assert!(resolve_port(None, Some("70000"), None).is_err());
        // an explicit CLI port wins even over a broken PORT
        assert!(resolve_port(Some(8080), Some("abc"), None).is_ok());
    }

    #[test]
    fn tailwind_section() {
        let c = ProjectConfig::parse("[tailwind]\nenabled = true\ninput = \"src/t.css\"").unwrap();
        assert!(c.tailwind.enabled);
        assert_eq!(c.tailwind.input.as_deref(), Some("src/t.css"));
        assert!(!ProjectConfig::default().tailwind.enabled);
        assert!(ProjectConfig::parse("[tailwind]\nenable = true").is_err());
    }

    #[test]
    fn config_parses_and_rejects_unknown_keys() {
        let c = ProjectConfig::parse(
            "[app]\nname=\"x\"\n[server]\nport=4000\nhost=\"0.0.0.0\"\n[dev]\nassets=\"public\"",
        )
        .unwrap();
        assert_eq!(c.server.port, Some(4000));
        assert_eq!(c.assets_dir(), "public");
        assert!(ProjectConfig::parse("[server]\nprt=1").is_err(), "typos are reported");
        assert_eq!(ProjectConfig::parse("").unwrap(), ProjectConfig::default());
    }

    #[test]
    fn project_root_and_package_name() {
        let dir = std::env::temp_dir().join(format!("yawn-cfg-{}", std::process::id()));
        let sub = dir.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), "").unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo-app\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("Cargo.lock"),
            "[[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.129\"\n",
        )
        .unwrap();
        assert_eq!(find_project_root(&sub).as_deref(), Some(dir.as_path()));
        assert_eq!(read_package_name(&dir).unwrap(), "demo-app");
        assert_eq!(locked_version(&dir, "wasm-bindgen").as_deref(), Some("0.2.129"));
        assert_eq!(locked_version(&dir, "nope"), None);
        assert!(find_project_root(Path::new("/")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
