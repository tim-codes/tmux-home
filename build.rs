//! Sets `TMUX_HOME_BUILD_ID`, the build identity the daemon handshake
//! compares (spec §3), so a rebuild at the same package version still
//! replaces a running daemon:
//!
//! - git, clean tree: `<version>+g<sha12>`
//! - git, dirty tree: `<version>+g<sha12>.dirty.<src12>`
//! - no git (a crates tarball, no `git` on PATH): `<version>+src.<src12>`
//!
//! `<src12>` hashes the sources that make up the binary (`src/**`,
//! `build.rs`, `Cargo.toml`, `Cargo.lock`), so two different dirty builds
//! of one commit differ too.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Hash of the binary's sources (path and contents, in path order).
fn source_hash(root: &Path) -> String {
    let mut paths = Vec::new();
    files(&root.join("src"), &mut paths);
    for f in ["build.rs", "Cargo.toml", "Cargo.lock"] {
        paths.push(root.join(f));
    }
    paths.sort();
    let mut h = sha1_smol::Sha1::new();
    for p in &paths {
        let rel = p.strip_prefix(root).unwrap_or(p);
        h.update(rel.to_string_lossy().as_bytes());
        h.update(&[0]);
        h.update(&std::fs::read(p).unwrap_or_default());
        h.update(&[0]);
    }
    h.digest().to_string()[..12].to_string()
}

/// The crate's git checkout, if the crate root is the top of one (not, say,
/// a crate unpacked inside some unrelated repository).
fn own_checkout(root: &Path) -> bool {
    let top = git(&["-C", root.to_str().unwrap(), "rev-parse", "--show-toplevel"]);
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    top.is_some_and(|t| canon(Path::new(&t)).is_some() && canon(Path::new(&t)) == canon(root))
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let r = root.to_str().unwrap();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    // Re-run when the sources change (the dirty hash) or the commit moves.
    // Only existing paths are named: cargo treats a missing one as changed
    // and would re-run (and recompile the crate) on every build.
    for f in ["src", "build.rs", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={f}");
    }
    let sha = own_checkout(&root)
        .then(|| git(&["-C", r, "rev-parse", "--short=12", "HEAD"]))
        .flatten();
    let id = match sha {
        Some(sha) if !sha.is_empty() => {
            // HEAD (a checkout) and the branch it names (a commit): the loose
            // ref, or packed-refs where it may live instead. Not the index:
            // any `git status` (a prompt, an editor) rewrites it.
            let mut watch = vec!["HEAD".to_string(), "packed-refs".into()];
            if let Some(b) = git(&["-C", r, "rev-parse", "--symbolic-full-name", "HEAD"])
                && b.starts_with("refs/")
            {
                watch.push(b);
            }
            for w in watch {
                if let Some(p) = git(&["-C", r, "rev-parse", "--git-path", &w]) {
                    let p = root.join(p);
                    if p.exists() {
                        println!("cargo:rerun-if-changed={}", p.display());
                    }
                }
            }
            // --no-optional-locks: never take index.lock from a build
            let dirty = git(&[
                "--no-optional-locks",
                "-C",
                r,
                "status",
                "--porcelain",
                "--untracked-files=normal",
                "--",
                "src",
                "build.rs",
                "Cargo.toml",
                "Cargo.lock",
            ])
            .is_some_and(|s| !s.is_empty());
            if dirty {
                format!("{version}+g{sha}.dirty.{}", source_hash(&root))
            } else {
                format!("{version}+g{sha}")
            }
        }
        _ => format!("{version}+src.{}", source_hash(&root)),
    };
    println!("cargo:rustc-env=TMUX_HOME_BUILD_ID={id}");
}
