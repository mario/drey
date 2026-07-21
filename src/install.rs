//! `drey install` / `drey uninstall`: interposing drey in front of every
//! language server on this machine, for every LSP client at once.
//!
//! This used to live in `scripts/install.sh`, which meant it was only reachable
//! from a clone of the repository. Someone who got the binary from Homebrew or
//! `cargo install` had no way to set up the `PATH` interposition at all, so the
//! logic moved here and the scripts became thin wrappers.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MARK_BEGIN: &str = "# >>> drey >>>";
const MARK_END: &str = "# <<< drey <<<";

/// Every wrapper carries this line, so uninstall can tell the files it wrote
/// from anything else that happens to live in the same directory.
const WRAPPER_MARK: &str = "# Installed by drey.";

/// A language server we know how to look for: the name it gets in the config,
/// the executable to find on `PATH`, and the markers that widen a client's root
/// to the real workspace root.
struct Known {
    name: &'static str,
    binary: &'static str,
    root_markers: &'static [&'static str],
}

/// The same list `scripts/install.sh` carried, in the same order.
const KNOWN: &[Known] = &[
    Known {
        name: "rust-analyzer",
        binary: "rust-analyzer",
        root_markers: &["Cargo.toml", "rust-project.json"],
    },
    Known {
        name: "gopls",
        binary: "gopls",
        root_markers: &["go.work", "go.mod"],
    },
    Known {
        name: "typescript-language-server",
        binary: "typescript-language-server",
        root_markers: &["pnpm-workspace.yaml", "package.json", "tsconfig.json"],
    },
    Known {
        name: "pyright-langserver",
        binary: "pyright-langserver",
        root_markers: &["pyproject.toml", "setup.py"],
    },
    Known {
        name: "ruff",
        binary: "ruff",
        root_markers: &["pyproject.toml", "ruff.toml"],
    },
    Known {
        name: "clangd",
        binary: "clangd",
        root_markers: &["compile_commands.json", "CMakeLists.txt"],
    },
    Known {
        name: "lua-language-server",
        binary: "lua-language-server",
        root_markers: &[".luarc.json"],
    },
    Known {
        name: "jdtls",
        binary: "jdtls",
        root_markers: &["pom.xml", "build.gradle"],
    },
    Known {
        name: "ruby-lsp",
        binary: "ruby-lsp",
        root_markers: &["Gemfile", ".ruby-lsp"],
    },
    Known {
        name: "intelephense",
        binary: "intelephense",
        root_markers: &["composer.json"],
    },
    Known {
        name: "kotlin-lsp",
        binary: "kotlin-lsp",
        root_markers: &["build.gradle.kts", "settings.gradle.kts"],
    },
    Known {
        name: "sourcekit-lsp",
        binary: "sourcekit-lsp",
        root_markers: &["Package.swift"],
    },
    Known {
        name: "zls",
        binary: "zls",
        root_markers: &["build.zig"],
    },
    Known {
        name: "solargraph",
        binary: "solargraph",
        root_markers: &["Gemfile"],
    },
    Known {
        name: "omnisharp",
        binary: "OmniSharp",
        root_markers: &["*.sln"],
    },
];

/// A language server found on this machine, with the absolute path the config
/// will point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Logical name, used for both the config section and the wrapper filename.
    pub name: String,
    /// Absolute path to the real executable.
    pub command: PathBuf,
    pub root_markers: Vec<String>,
}

/// Where the wrapper scripts go. Hardcoded rather than XDG-derived because the
/// `PATH` line written into the shell rc files has to name it literally.
pub fn wrapper_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".drey/bin")
}

/// Entry point for `drey install`.
pub fn install(dry_run: bool, force: bool) -> Result<()> {
    let bin_dir = wrapper_dir();
    let config = crate::config::config_path();
    let exe = current_exe(&bin_dir)?;

    println!();
    println!("drey install{}", if dry_run { " (dry run)" } else { "" });
    println!();
    say(&format!("using binary: {}", exe.display()));
    println!();

    let found = discover(&bin_dir);
    for f in &found {
        say(&format!("found {} -> {}", f.name, f.command.display()));
    }
    if found.is_empty() {
        say("no language servers found on PATH; nothing to interpose");
    }
    println!();

    let stamp = timestamp();
    let toml = render_config(&found);

    if dry_run {
        say(&format!("would write {}", config.display()));
        say(&format!(
            "would write {} wrapper(s) to {}",
            found.len(),
            bin_dir.display()
        ));
        for rc in rc_files() {
            match read_optional(&rc)? {
                Some(text) if text.contains(MARK_BEGIN) => {
                    say(&format!("PATH entry already present in {}", rc.display()))
                }
                _ => say(&format!("would add PATH entry to {}", rc.display())),
            }
        }
        println!();
        say("nothing was changed");
        println!();
        return Ok(());
    }

    write_config(&config, &toml, &stamp)?;
    say(&format!("wrote {}", config.display()));

    let written = write_wrappers(&bin_dir, &exe, &found, force)?;
    say(&format!(
        "wrote {written} wrapper(s) to {}",
        bin_dir.display()
    ));

    for rc in rc_files() {
        match add_path_block(&rc, &stamp)? {
            true => say(&format!("added PATH entry to {}", rc.display())),
            false => say(&format!("PATH entry already present in {}", rc.display())),
        }
    }

    println!();
    println!("Done. Open a new shell, or run:");
    println!("    export PATH=\"$HOME/.drey/bin:$PATH\"");
    println!();
    println!("Check it is working:");
    println!(
        "    which rust-analyzer      # should be {}/rust-analyzer",
        bin_dir.display()
    );
    println!("    drey status              # shows shared servers once a client connects");
    println!();
    println!("To revert everything:");
    println!("    drey uninstall");
    println!();
    Ok(())
}

/// Entry point for `drey uninstall`.
pub fn uninstall(dry_run: bool) -> Result<()> {
    let bin_dir = wrapper_dir();
    let config = crate::config::config_path();

    println!();
    println!("drey uninstall{}", if dry_run { " (dry run)" } else { "" });
    println!();

    let wrappers = drey_wrappers(&bin_dir)?;
    for w in &wrappers {
        if dry_run {
            say(&format!("would remove {}", w.display()));
        } else {
            std::fs::remove_file(w).with_context(|| format!("removing {}", w.display()))?;
            say(&format!("removed {}", w.display()));
        }
    }
    if wrappers.is_empty() {
        say("no wrappers to remove");
    }

    let stamp = timestamp();
    for rc in rc_files_for_removal() {
        let Some(text) = read_optional(&rc)? else {
            continue;
        };
        if !text.contains(MARK_BEGIN) {
            continue;
        }
        if dry_run {
            say(&format!("would remove PATH entry from {}", rc.display()));
            continue;
        }
        backup(&rc, &stamp)?;
        std::fs::write(&rc, strip_path_block(&text))
            .with_context(|| format!("rewriting {}", rc.display()))?;
        say(&format!(
            "removed PATH entry from {} (backup alongside it)",
            rc.display()
        ));
    }

    println!();
    say(&format!(
        "left {} in place; delete it yourself if you want it gone",
        config.display()
    ));
    println!();
    println!("Open a new shell so PATH is rebuilt, then confirm:");
    println!("    which rust-analyzer     # should be the real one again");
    println!();
    Ok(())
}

fn say(s: &str) {
    println!("  {s}");
}

/// The path wrappers must exec: wherever this binary actually is, so a copy
/// from Homebrew or `cargo install` works untouched.
///
/// `~/.drey/bin/drey` is fine — that is where `scripts/install.sh` puts it, and
/// wrappers are named after servers, never `drey`. Any *other* name inside the
/// wrapper directory means we are running as a wrapper, and a wrapper pointing
/// at itself would exec forever, so refuse rather than write that out.
fn current_exe(bin_dir: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running drey binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let bin_dir = bin_dir
        .canonicalize()
        .unwrap_or_else(|_| bin_dir.to_owned());
    anyhow::ensure!(
        exe.parent() != Some(bin_dir.as_path()) || exe.file_name().is_some_and(|n| n == "drey"),
        "the running binary {} is inside the wrapper directory {}; \
         wrappers pointing there would exec themselves",
        exe.display(),
        bin_dir.display()
    );
    Ok(exe)
}

fn discover(bin_dir: &Path) -> Vec<Found> {
    let entries = path_entries();
    KNOWN
        .iter()
        .filter_map(|k| {
            let hit = resolve_on_path(k.binary, &entries, bin_dir)?;
            Some(Found {
                name: k.name.to_string(),
                command: resolve_asdf_shim(k.binary, hit),
                root_markers: k.root_markers.iter().map(|s| s.to_string()).collect(),
            })
        })
        .collect()
}

fn path_entries() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// First executable named `binary` in `entries`, skipping anything inside
/// `skip_dir`. Our own wrappers live there and resolving to one would make the
/// config point drey at itself.
pub fn resolve_on_path(binary: &str, entries: &[PathBuf], skip_dir: &Path) -> Option<PathBuf> {
    entries
        .iter()
        .filter(|dir| dir.as_os_str() != skip_dir.as_os_str())
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// asdf puts a shell shim on `PATH` that dispatches by the current directory.
/// Recording the shim would make every workspace resolve to whatever version
/// asdf picked at daemon start, so ask asdf for the real binary instead.
fn resolve_asdf_shim(binary: &str, path: PathBuf) -> PathBuf {
    if !path.to_string_lossy().contains("/.asdf/shims/") {
        return path;
    }
    let Ok(out) = std::process::Command::new("asdf")
        .arg("which")
        .arg(binary)
        .output()
    else {
        return path;
    };
    let real = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !real.is_empty() {
        PathBuf::from(real)
    } else {
        path
    }
}

/// Renders the whole config file. Commands are absolute paths, so the wrappers
/// on `PATH` can never make drey re-enter itself.
pub fn render_config(found: &[Found]) -> String {
    let mut out = String::from(
        "# Generated by `drey install`.\n\
         # Commands are ABSOLUTE paths to the real servers, so the PATH wrappers\n\
         # in ~/.drey/bin can never cause drey to re-enter itself.\n\
         \n\
         [daemon]\n\
         idle_timeout_secs = 1800\n\
         \n",
    );
    // Deterministic order regardless of PATH layout, so re-running install
    // produces a byte-identical file and diffs against the backup stay small.
    let sorted: BTreeMap<&str, &Found> = found.iter().map(|f| (f.name.as_str(), f)).collect();
    for f in sorted.values() {
        out.push_str(&format!("[server.{}]\n", f.name));
        out.push_str(&format!("command = \"{}\"\n", f.command.display()));
        if !f.root_markers.is_empty() {
            let markers: Vec<String> = f.root_markers.iter().map(|m| format!("\"{m}\"")).collect();
            out.push_str(&format!("root_markers = [{}]\n", markers.join(", ")));
        }
        out.push('\n');
    }
    out
}

/// The two-line `sh` wrapper a client actually executes.
pub fn render_wrapper(name: &str, exe: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         {WRAPPER_MARK} Routes this language server through the shared daemon.\n\
         # Remove this file (or run `drey uninstall`) to go back to the\n\
         # real `{name}` on PATH.\n\
         exec \"{}\" serve \"{name}\" \"$@\"\n",
        exe.display()
    )
}

fn write_config(path: &Path, body: &str, stamp: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if path.exists() {
        let to = backup(path, stamp)?;
        say(&format!("backed up existing config to {}", to.display()));
    }
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

fn write_wrappers(bin_dir: &Path, exe: &Path, found: &[Found], force: bool) -> Result<usize> {
    std::fs::create_dir_all(bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    let mut written = 0;
    for f in found {
        let path = bin_dir.join(&f.name);
        // A file we did not write is somebody else's; replacing it silently
        // would remove a real binary from their PATH.
        if !force && path.exists() && !is_drey_wrapper(&path) {
            say(&format!(
                "skipping {} (not a drey wrapper; use --force to replace)",
                path.display()
            ));
            continue;
        }
        std::fs::write(&path, render_wrapper(&f.name, exe))
            .with_context(|| format!("writing {}", path.display()))?;
        set_executable(&path)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod 0755 {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn is_drey_wrapper(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|t| t.contains(WRAPPER_MARK))
}

fn drey_wrappers(bin_dir: &Path) -> Result<Vec<PathBuf>> {
    let Ok(dir) = std::fs::read_dir(bin_dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in dir {
        let path = entry
            .with_context(|| format!("listing {}", bin_dir.display()))?
            .path();
        if path.is_file() && is_drey_wrapper(&path) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// The block goes in BOTH zsh startup files, and it has to be in both:
///
///   `.zshenv` is read by every zsh, including the non-interactive ones that GUI
///             editors, launchd jobs and scripts use to spawn a language server.
///             `.zshrc` alone would miss all of those.
///   `.zshrc`  is read after `.zshenv` for interactive shells, and version
///             managers (asdf, rbenv, nvm) prepend their own shims there.
///             Without a second prepend afterwards, those shims sit in front of
///             ours again.
///
/// Prepending twice is harmless: the duplicate PATH entry resolves identically.
fn rc_files() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let zsh = home.join(".zshenv").exists()
        || home.join(".zshrc").exists()
        || std::env::var_os("ZSH_VERSION").is_some();
    if zsh {
        vec![home.join(".zshenv"), home.join(".zshrc")]
    } else {
        vec![home.join(".bashrc")]
    }
}

/// Uninstall looks wider than install writes: an older install, or a different
/// shell at the time, may have left the block somewhere else.
fn rc_files_for_removal() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    [".zshenv", ".zshrc", ".bashrc", ".profile"]
        .iter()
        .map(|f| home.join(f))
        .collect()
}

/// Appends the marked block unless it is already there. Returns whether it
/// wrote anything.
fn add_path_block(rc: &Path, stamp: &str) -> Result<bool> {
    if let Some(text) = read_optional(rc)? {
        if text.contains(MARK_BEGIN) {
            return Ok(false);
        }
        backup(rc, stamp)?;
        std::fs::write(rc, append_path_block(&text))
            .with_context(|| format!("appending to {}", rc.display()))?;
    } else {
        std::fs::write(rc, append_path_block(""))
            .with_context(|| format!("creating {}", rc.display()))?;
    }
    Ok(true)
}

/// Pure form of the append, so the shape of the block is testable without a
/// real home directory.
pub fn append_path_block(existing: &str) -> String {
    format!(
        "{existing}\n{MARK_BEGIN}\n\
         # Shared language servers. Remove this block to disable drey.\n\
         export PATH=\"$HOME/.drey/bin:$PATH\"\n\
         {MARK_END}\n"
    )
}

/// Removes the marked block, and the blank line that preceded it.
pub fn strip_path_block(existing: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in existing.lines() {
        if line == MARK_BEGIN {
            skipping = true;
            if out.last() == Some(&"") {
                out.pop();
            }
            continue;
        }
        if line == MARK_END {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push(line);
        }
    }
    if out.is_empty() {
        return String::new();
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn backup(path: &Path, stamp: &str) -> Result<PathBuf> {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".backup-{stamp}"));
    let to = PathBuf::from(name);
    std::fs::copy(path, &to)
        .with_context(|| format!("backing up {} to {}", path.display(), to.display()))?;
    Ok(to)
}

/// `YYYYMMDD-HHMMSS` in UTC, computed from the epoch so no date crate is
/// needed for a filename suffix.
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

/// Howard Hinnant's days-from-civil, inverted. Exact for every date we can
/// reach, and cheaper than a dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn touch_exe(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    #[cfg(unix)]
    fn path_scan_takes_the_first_executable_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let first = touch_exe(&a, "gopls");
        touch_exe(&b, "gopls");
        let entries = vec![a, b];
        assert_eq!(
            resolve_on_path("gopls", &entries, Path::new("/nonexistent")),
            Some(first)
        );
    }

    #[test]
    #[cfg(unix)]
    fn path_scan_skips_our_own_wrapper_directory() {
        // Without this, install would record ~/.drey/bin/gopls as the "real"
        // gopls and the daemon would exec the wrapper that called it.
        let tmp = tempfile::tempdir().unwrap();
        let (wrappers, real) = (tmp.path().join("drey-bin"), tmp.path().join("real"));
        std::fs::create_dir_all(&wrappers).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        touch_exe(&wrappers, "gopls");
        let expected = touch_exe(&real, "gopls");
        let entries = vec![wrappers.clone(), real];
        assert_eq!(
            resolve_on_path("gopls", &entries, &wrappers),
            Some(expected)
        );
    }

    #[test]
    #[cfg(unix)]
    fn path_scan_ignores_non_executable_files_of_the_right_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("zls"), "not a program").unwrap();
        let entries = vec![tmp.path().to_path_buf()];
        assert_eq!(resolve_on_path("zls", &entries, Path::new("/nope")), None);
    }

    #[test]
    fn path_scan_returns_nothing_when_the_binary_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![tmp.path().to_path_buf()];
        assert_eq!(
            resolve_on_path("no-such-server", &entries, Path::new("/nope")),
            None
        );
    }

    fn found(name: &str, command: &str, markers: &[&str]) -> Found {
        Found {
            name: name.to_string(),
            command: PathBuf::from(command),
            root_markers: markers.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn generated_config_parses_back_into_the_real_config_type() {
        let text = render_config(&[
            found("gopls", "/usr/local/bin/gopls", &["go.work", "go.mod"]),
            found("zls", "/opt/zls", &["build.zig"]),
        ]);
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.daemon.idle_timeout_secs, 1800);
        assert_eq!(cfg.server["gopls"].command, "/usr/local/bin/gopls");
        assert_eq!(cfg.server["gopls"].root_markers, ["go.work", "go.mod"]);
        assert_eq!(cfg.server["zls"].command, "/opt/zls");
    }

    #[test]
    fn generated_config_records_absolute_paths_only() {
        let text = render_config(&[found("ruff", "/usr/bin/ruff", &["ruff.toml"])]);
        assert!(text.contains("command = \"/usr/bin/ruff\""), "{text}");
    }

    #[test]
    fn a_config_with_no_servers_is_still_valid() {
        let cfg: crate::config::Config = toml::from_str(&render_config(&[])).unwrap();
        assert_eq!(cfg.daemon.idle_timeout_secs, 1800);
    }

    #[test]
    fn wrapper_execs_the_binary_it_was_given() {
        let w = render_wrapper("gopls", Path::new("/opt/homebrew/bin/drey"));
        assert!(w.starts_with("#!/bin/sh\n"), "{w}");
        assert!(
            w.contains("exec \"/opt/homebrew/bin/drey\" serve \"gopls\" \"$@\""),
            "{w}"
        );
        assert!(w.contains(WRAPPER_MARK), "{w}");
    }

    #[test]
    fn the_rc_block_prepends_the_wrapper_directory() {
        let out = append_path_block("export EDITOR=vi\n");
        assert!(out.contains(MARK_BEGIN) && out.contains(MARK_END), "{out}");
        assert!(
            out.contains("export PATH=\"$HOME/.drey/bin:$PATH\""),
            "{out}"
        );
        assert!(out.starts_with("export EDITOR=vi\n"), "{out}");
    }

    #[test]
    fn stripping_the_block_restores_the_original_file() {
        let original = "export EDITOR=vi\n";
        assert_eq!(strip_path_block(&append_path_block(original)), original);
    }

    #[test]
    fn stripping_leaves_a_file_without_the_block_alone() {
        assert_eq!(strip_path_block("export A=1\n"), "export A=1\n");
    }

    #[test]
    fn stripping_keeps_lines_that_follow_the_block() {
        let text = format!("{}source ~/other\n", append_path_block("export A=1\n"));
        assert_eq!(strip_path_block(&text), "export A=1\nsource ~/other\n");
    }

    #[test]
    fn the_block_is_added_once_and_removed_once_on_a_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path().join(".zshenv");
        std::fs::write(&rc, "export A=1\n").unwrap();

        assert!(add_path_block(&rc, "20260101-000000").unwrap());
        assert!(!add_path_block(&rc, "20260101-000000").unwrap());
        let text = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(text.matches(MARK_BEGIN).count(), 1, "{text}");
        // The pre-existing content was backed up before being touched.
        let backup = tmp.path().join(".zshenv.backup-20260101-000000");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "export A=1\n");

        std::fs::write(&rc, strip_path_block(&text)).unwrap();
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "export A=1\n");
    }

    #[test]
    fn only_files_carrying_the_marker_count_as_wrappers() {
        let tmp = tempfile::tempdir().unwrap();
        let ours = tmp.path().join("gopls");
        std::fs::write(&ours, render_wrapper("gopls", Path::new("/bin/drey"))).unwrap();
        std::fs::write(tmp.path().join("something-else"), "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(drey_wrappers(tmp.path()).unwrap(), vec![ours]);
    }

    #[test]
    fn a_missing_wrapper_directory_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(drey_wrappers(&tmp.path().join("absent"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn wrappers_are_written_executable_and_never_clobber_a_stranger() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("ruff"), "#!/bin/sh\nreal ruff\n").unwrap();

        let servers = [found("ruff", "/usr/bin/ruff", &["ruff.toml"])];
        let exe = Path::new("/opt/drey");
        assert_eq!(write_wrappers(&bin, exe, &servers, false).unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(bin.join("ruff")).unwrap(),
            "#!/bin/sh\nreal ruff\n"
        );

        assert_eq!(write_wrappers(&bin, exe, &servers, true).unwrap(), 1);
        assert!(is_drey_wrapper(&bin.join("ruff")));
        assert!(is_executable(&bin.join("ruff")));
    }

    #[test]
    fn the_backup_suffix_is_a_sortable_timestamp() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert!(stamp.chars().nth(8) == Some('-'), "{stamp}");
        assert!(stamp.starts_with("20"), "{stamp}");
    }

    #[test]
    fn the_calendar_conversion_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // A leap day, where a naive 365-day approximation would drift.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn every_known_server_has_markers_and_a_usable_name() {
        for k in KNOWN {
            assert!(!k.root_markers.is_empty(), "{} has no markers", k.name);
            assert!(!k.name.contains('/'), "{} is not a filename", k.name);
        }
    }
}
