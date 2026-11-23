//! `scripts/check-plugin.sh` refuses each way the plugin's wiring can break,
//! naming that one: it runs in a copy of what it reads (Cargo.toml,
//! src/relay.rs, plugins/), mutated once per case.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// A copy of what the script reads, removed when dropped (a failed assertion
/// included).
struct Copy(PathBuf);

impl Copy {
    fn new(name: &str) -> Copy {
        let dir = std::env::temp_dir().join(format!(
            "fleet-lsp-checkplugin-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir); // a leftover copy is replaced below
        for rel in ["Cargo.toml", "src/relay.rs", "scripts/check-plugin.sh"] {
            let to = dir.join(rel);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(Path::new(ROOT).join(rel), to).unwrap();
        }
        copy_tree(&Path::new(ROOT).join("plugins"), &dir.join("plugins"));
        Copy(dir)
    }

    fn edit(&self, rel: &str, from: &str, to: &str) {
        let path = self.0.join(rel);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{rel} holds no {from:?}");
        fs::write(&path, text.replacen(from, to, 1)).unwrap();
    }

    /// The script's exit code and its stderr lines.
    fn check(&self) -> (Option<i32>, Vec<String>) {
        let out = Command::new("sh")
            .arg("scripts/check-plugin.sh")
            .current_dir(&self.0)
            .output()
            .unwrap();
        let err = String::from_utf8(out.stderr).unwrap();
        (out.status.code(), err.lines().map(str::to_string).collect())
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0); // a leftover copy does not fail a test
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

const HOOKS: &str = "plugins/fleet-lsp/hooks/hooks.json";

#[test]
fn the_plugin_as_it_is_passes() {
    let copy = Copy::new("as-is");
    assert_eq!(copy.check(), (Some(0), Vec::new()));
}

/// FALSIFY: drop the PostToolUse check from check-plugin.sh.
#[test]
fn a_posttooluse_command_is_refused() {
    let copy = Copy::new("posttooluse");
    copy.edit(HOOKS, r#""hooks": {"#, r#""hooks": { "PostToolUse": [], "#);
    assert_eq!(
        copy.check(),
        (
            Some(1),
            vec!["hooks.json has a PostToolUse command: first contact is the mod's".to_string()]
        )
    );
}

#[test]
fn a_missing_module_is_refused() {
    let copy = Copy::new("module");
    fs::remove_file(copy.0.join("plugins/fleet-lsp/hooks/register.ts")).unwrap();
    assert_eq!(
        copy.check(),
        (
            Some(1),
            vec![
                "hooks.json modules names no file under plugins/fleet-lsp/hooks: 'register.ts'"
                    .to_string()
            ]
        )
    );
}

#[test]
fn a_plugin_version_apart_from_the_binary_is_refused() {
    let copy = Copy::new("version");
    let manifest = "plugins/fleet-lsp/.claude-plugin/plugin.json";
    let version = env!("CARGO_PKG_VERSION");
    copy.edit(
        manifest,
        &format!("\"version\": \"{version}\""),
        "\"version\": \"9.9.9\"",
    );
    assert_eq!(
        copy.check(),
        (
            Some(1),
            vec![format!("plugin.json version is not {version}")]
        )
    );
}
