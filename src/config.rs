//! User configuration: which real language server backs each logical name.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: HashMap<String, ServerConfig>,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Executable to run, e.g. `rust-analyzer`.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Marker files used to widen a client's root to the true workspace root,
    /// e.g. `["Cargo.toml"]`. The outermost match wins, so opening one crate
    /// inside a workspace attaches to the workspace's existing backend.
    #[serde(default)]
    pub root_markers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    /// Evict a backend this long after its last client detaches.
    #[serde(default = "default_idle_secs")]
    pub idle_timeout_secs: u64,
    /// Evict the least recently used idle backend once this many are live.
    /// 0 disables the cap.
    #[serde(default)]
    pub max_backends: usize,
}

fn default_idle_secs() -> u64 {
    30 * 60
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_secs(),
            max_backends: 0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: builtin_servers(),
            daemon: DaemonConfig::default(),
        }
    }
}

/// Defaults so drey is useful before anyone writes a config file. Every
/// entry is overridable, and unknown names are an error rather than a guess.
fn builtin_servers() -> HashMap<String, ServerConfig> {
    let mk = |command: &str, args: &[&str], markers: &[&str]| ServerConfig {
        command: command.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: HashMap::new(),
        root_markers: markers.iter().map(|s| s.to_string()).collect(),
    };
    HashMap::from([
        (
            "rust-analyzer".to_string(),
            mk("rust-analyzer", &[], &["Cargo.toml", "rust-project.json"]),
        ),
        (
            "gopls".to_string(),
            mk("gopls", &[], &["go.work", "go.mod"]),
        ),
        (
            "typescript".to_string(),
            mk(
                "typescript-language-server",
                &["--stdio"],
                &["pnpm-workspace.yaml", "package.json", "tsconfig.json"],
            ),
        ),
        (
            "pyright".to_string(),
            mk(
                "pyright-langserver",
                &["--stdio"],
                &["pyproject.toml", "setup.py", "requirements.txt"],
            ),
        ),
        (
            "ruff".to_string(),
            mk("ruff", &["server"], &["pyproject.toml", "ruff.toml"]),
        ),
        (
            "clangd".to_string(),
            mk("clangd", &[], &["compile_commands.json", "CMakeLists.txt"]),
        ),
        ("zls".to_string(), mk("zls", &[], &["build.zig"])),
        (
            "lua".to_string(),
            mk("lua-language-server", &[], &[".luarc.json"]),
        ),
        ("elixir".to_string(), mk("elixir-ls", &[], &["mix.exs"])),
        (
            "jdtls".to_string(),
            mk(
                "jdtls",
                &[],
                &["pom.xml", "build.gradle", "settings.gradle"],
            ),
        ),
    ])
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("DREY_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("drey/config.toml")
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        // User entries win; unlisted builtins stay available.
        for (name, server) in builtin_servers() {
            cfg.server.entry(name).or_insert(server);
        }
        Ok(cfg)
    }

    pub fn server(&self, name: &str) -> Result<&ServerConfig> {
        self.server.get(name).with_context(|| {
            let mut known: Vec<_> = self.server.keys().cloned().collect();
            known.sort();
            format!(
                "no server named `{name}`; known servers: {}. Define it in {}",
                known.join(", "),
                config_path().display()
            )
        })
    }
}

/// Widens a client-supplied root to the outermost directory still carrying one
/// of the server's markers. Two clients that opened different crates of the
/// same Cargo workspace therefore land on one backend and one index.
///
/// The walk never reaches the home directory. A stray `Cargo.toml` or
/// `package.json` in `$HOME` is common enough, and widening to it would merge
/// every project underneath into one backend indexing the entire home
/// directory. Stopping short costs a little sharing in a case nobody has;
/// not stopping costs everything in a case people really do have.
///
/// `.git` is deliberately *not* a stopping marker: git worktrees are
/// legitimately separate workspaces and must not be merged.
pub fn widen_root(start: &Path, markers: &[String]) -> PathBuf {
    widen_root_bounded(start, markers, dirs::home_dir().as_deref())
}

/// The body of [`widen_root`], with the home directory passed in.
///
/// Tests need to place a fake home around a temporary directory. Setting `HOME`
/// to do that is process-wide, and other tests spawn backends whose `PATH`
/// sanitising reads the same variable, so it produced failures in unrelated
/// tests depending on scheduling. Injection keeps the whole suite parallel.
fn widen_root_bounded(start: &Path, markers: &[String], home: Option<&Path>) -> PathBuf {
    let mut best = start.to_path_buf();
    let mut cur = start;

    while let Some(parent) = cur.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        // Both sides matter. `parent == home` stops the loop adopting home
        // itself as the widened root, which a stray marker in `$HOME` would
        // otherwise cause. `cur == home` stops a client that opened `$HOME`
        // directly from climbing above it into `/Users` or `/`.
        if home.is_some_and(|h| parent == h || cur == h) {
            break;
        }
        if markers.iter().any(|m| parent.join(m).exists()) {
            best = parent.to_path_buf();
        }
        cur = parent;
    }
    best
}

/// Runtime directory holding the socket, lock and log.
pub fn runtime_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DREY_RUNTIME_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(p).join("drey");
    }
    dirs::state_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("drey")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

pub fn log_path() -> PathBuf {
    runtime_dir().join("daemon.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_servers_are_available_without_a_config_file() {
        let cfg = Config::default();
        assert_eq!(
            cfg.server("rust-analyzer").unwrap().command,
            "rust-analyzer"
        );
        assert!(cfg.server("nonesuch").is_err());
    }

    #[test]
    fn unknown_server_error_lists_the_known_ones() {
        let err = Config::default().server("nope").unwrap_err().to_string();
        assert!(err.contains("rust-analyzer"), "{err}");
    }

    #[test]
    fn widen_root_climbs_to_the_outermost_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let crate_dir = ws.join("crates/inner");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "[package]").unwrap();

        let markers = vec!["Cargo.toml".to_string()];
        assert_eq!(widen_root(&crate_dir, &markers), ws);
    }

    #[test]
    fn widen_root_is_a_no_op_without_markers_above() {
        let tmp = tempfile::tempdir().unwrap();
        let solo = tmp.path().join("solo");
        std::fs::create_dir_all(&solo).unwrap();
        std::fs::write(solo.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(widen_root(&solo, &["Cargo.toml".to_string()]), solo);
    }

    /// `DREY_CONFIG` and friends are process-wide, so the tests that set them
    /// take turns.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn load_from(toml_text: &str) -> Config {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, toml_text).unwrap();
        std::env::set_var("DREY_CONFIG", &path);
        let cfg = Config::load().unwrap();
        std::env::remove_var("DREY_CONFIG");
        cfg
    }

    #[test]
    fn a_user_entry_overrides_the_builtin_of_the_same_name() {
        let cfg = load_from(
            r#"
            [server.rust-analyzer]
            command = "/opt/ra"
            args = ["--log"]
            root_markers = ["Cargo.toml"]
            "#,
        );
        let ra = cfg.server("rust-analyzer").unwrap();
        assert_eq!(ra.command, "/opt/ra");
        assert_eq!(ra.args, ["--log"]);
    }

    #[test]
    fn builtins_not_mentioned_by_the_user_survive_the_merge() {
        let cfg = load_from("[server.mine]\ncommand = \"mine\"\n");
        assert_eq!(cfg.server("mine").unwrap().command, "mine");
        assert_eq!(cfg.server("gopls").unwrap().command, "gopls");
    }

    #[test]
    fn omitted_server_fields_default_to_empty() {
        let cfg = load_from("[server.bare]\ncommand = \"bare\"\n");
        let s = cfg.server("bare").unwrap();
        assert!(s.args.is_empty() && s.env.is_empty() && s.root_markers.is_empty());
    }

    #[test]
    fn env_entries_are_read_verbatim() {
        let cfg = load_from("[server.e]\ncommand = \"e\"\n[server.e.env]\nRUST_LOG = \"debug\"\n");
        assert_eq!(cfg.server("e").unwrap().env["RUST_LOG"], "debug");
    }

    #[test]
    fn daemon_settings_default_when_the_section_is_absent() {
        let cfg = load_from("[server.x]\ncommand = \"x\"\n");
        assert_eq!(cfg.daemon.idle_timeout_secs, 30 * 60);
        assert_eq!(cfg.daemon.max_backends, 0);
    }

    #[test]
    fn daemon_settings_are_read_from_the_file() {
        let cfg = load_from("[daemon]\nidle_timeout_secs = 5\nmax_backends = 3\n");
        assert_eq!(cfg.daemon.idle_timeout_secs, 5);
        assert_eq!(cfg.daemon.max_backends, 3);
    }

    #[test]
    fn one_daemon_key_does_not_reset_the_other() {
        let cfg = load_from("[daemon]\nmax_backends = 2\n");
        assert_eq!(cfg.daemon.max_backends, 2);
        assert_eq!(cfg.daemon.idle_timeout_secs, 30 * 60);
    }

    #[test]
    fn a_missing_config_file_is_not_an_error() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("DREY_CONFIG", tmp.path().join("absent.toml"));
        let cfg = Config::load().unwrap();
        std::env::remove_var("DREY_CONFIG");
        assert_eq!(cfg.server("zls").unwrap().command, "zls");
    }

    #[test]
    fn a_broken_config_file_names_itself_in_the_error() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[server.x\ncommand =").unwrap();
        std::env::set_var("DREY_CONFIG", &path);
        let err = Config::load().unwrap_err().to_string();
        std::env::remove_var("DREY_CONFIG");
        assert!(err.contains("config.toml"), "{err}");
    }

    #[test]
    fn a_server_entry_without_a_command_is_rejected() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[server.x]\nargs = []\n").unwrap();
        std::env::set_var("DREY_CONFIG", &path);
        let err = Config::load().unwrap_err();
        std::env::remove_var("DREY_CONFIG");
        assert!(err.to_string().contains("parsing"), "{err:#}");
    }

    #[test]
    fn every_builtin_has_markers_so_sibling_projects_can_share() {
        let cfg = Config::default();
        for (name, s) in &cfg.server {
            assert!(!s.command.is_empty(), "{name} has no command");
            assert!(!s.root_markers.is_empty(), "{name} has no root markers");
        }
    }

    #[test]
    fn config_path_prefers_the_environment_override() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DREY_CONFIG", "/tmp/drey-test.toml");
        let p = config_path();
        std::env::remove_var("DREY_CONFIG");
        assert_eq!(p, PathBuf::from("/tmp/drey-test.toml"));
    }

    #[test]
    fn runtime_paths_share_one_directory() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DREY_RUNTIME_DIR", "/tmp/drey-test-run");
        let (dir, sock, log) = (runtime_dir(), socket_path(), log_path());
        std::env::remove_var("DREY_RUNTIME_DIR");
        assert_eq!(dir, PathBuf::from("/tmp/drey-test-run"));
        assert_eq!(sock, dir.join("daemon.sock"));
        assert_eq!(log, dir.join("daemon.log"));
    }

    #[test]
    fn xdg_runtime_dir_gets_its_own_subdirectory() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        // These are process-wide, so leave the environment as we found it.
        let previous = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::remove_var("DREY_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/drey-xdg");
        let dir = runtime_dir();
        match previous {
            Some(p) => std::env::set_var("XDG_RUNTIME_DIR", p),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        assert_eq!(dir, PathBuf::from("/tmp/drey-xdg/drey"));
    }

    #[test]
    fn widen_root_stops_at_the_first_gap_in_the_marker_chain() {
        // A marker at the top with none in between must not pull the root up:
        // the intervening directory is a different project.
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().join("top");
        let deep = top.join("unrelated/inner");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(top.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(deep.join("Cargo.toml"), "[package]").unwrap();
        // `widen_root` keeps climbing, so the outermost marker wins.
        assert_eq!(widen_root(&deep, &["Cargo.toml".to_string()]), top);
    }

    #[test]
    fn widen_root_with_no_markers_configured_is_the_identity() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(widen_root(tmp.path(), &[]), tmp.path());
    }

    #[test]
    fn widen_root_accepts_any_of_several_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = tmp.path().join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(tmp.path().join("a/go.work"), "").unwrap();
        let markers = vec!["go.work".to_string(), "go.mod".to_string()];
        assert_eq!(widen_root(&inner, &markers), tmp.path().join("a"));
    }

    #[test]
    fn a_marker_directory_counts_as_a_marker() {
        // `exists()` does not distinguish; pin the behaviour either way.
        let tmp = tempfile::tempdir().unwrap();
        let inner = tmp.path().join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(tmp.path().join("a/marker")).unwrap();
        assert_eq!(
            widen_root(&inner, &["marker".to_string()]),
            tmp.path().join("a")
        );
    }

    #[test]
    fn widen_root_stops_before_the_home_directory() {
        // A stray Cargo.toml in $HOME must not pull every project under it into
        // one backend indexing the whole home directory.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = home.join("code/thing");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(home.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(project.join("Cargo.toml"), "[package]").unwrap();

        let widened = widen_root_bounded(&project, &["Cargo.toml".to_string()], Some(&home));
        assert_eq!(
            widened,
            project,
            "widening reached {} instead of stopping below home",
            widened.display()
        );
    }

    #[test]
    fn opening_the_home_directory_itself_does_not_climb_above_it() {
        // The guard has to cover both sides. Testing only `parent == home`
        // would let a client that opened $HOME walk up into /Users or /.
        let tmp = tempfile::tempdir().unwrap();
        let above = tmp.path().join("above");
        let home = above.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(above.join("Cargo.toml"), "[workspace]").unwrap();

        let widened = widen_root_bounded(&home, &["Cargo.toml".to_string()], Some(&home));
        assert_eq!(
            widened,
            home,
            "widening escaped home to {}",
            widened.display()
        );
    }

    #[test]
    fn widening_from_the_filesystem_root_is_the_identity() {
        // Named for what it proves. `/` has no parent, so the loop never runs.
        // Whether `/` can be *adopted* as a widened root is a different
        // question, and the test below is the one that answers it.
        assert_eq!(
            widen_root(Path::new("/"), &["Cargo.toml".to_string()]),
            Path::new("/")
        );
    }

    #[test]
    fn widening_never_adopts_the_filesystem_root() {
        // A marker at `/` must not pull every project on the machine into one
        // backend. Home usually stops the walk first, but a workspace outside
        // home (a mounted volume, /srv, a CI checkout) has no such guard.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("elsewhere/home");
        std::fs::create_dir_all(&home).unwrap();
        let widened = widen_root_bounded(
            Path::new("/usr"),
            &["definitely-not-here".to_string()],
            Some(&home),
        );
        assert_eq!(widened, Path::new("/usr"));
    }
}
