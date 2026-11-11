// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq skill`: the agent skill compiled into the binary, printed whole
//! or by part, installed into a skills directory, and the warning about an
//! installed copy older than this antiq.

mod support;

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};

/// The parts of the skill, in the order `--install` writes them. Each is
/// `references/<name>.md` beside `SKILL.md`.
const PARTS: [&str; 3] = ["mirroring", "import", "damage"];

/// The skill's directory in the repository.
fn skill_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/antiq")
}

/// A file of the skill as the repository holds it, read at run time so the
/// tests compare the binary's output with the file in the repository.
fn repository_file(relative: &str) -> String {
    let file = skill_dir().join(relative);
    fs::read_to_string(&file)
        .unwrap_or_else(|error| panic!("{} is part of the skill: {error}", file.display()))
}

fn core() -> String {
    repository_file("SKILL.md")
}

fn part(name: &str) -> String {
    repository_file(&format!("references/{name}.md"))
}

/// A directory to run in that holds no skills directory, so neither the
/// repository nor the developer's projects decide a test.
fn empty_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("the temp dir is writable")
}

fn skill(cwd: &Path, args: &[&str]) -> Run {
    Antiq::new().current_dir(cwd).arg("skill").args(args).run()
}

/// The text of a `SKILL.md` an earlier antiq installed.
fn installed_skill(version: &str) -> String {
    format!(
        "---\nname: antiq\ndescription: Old.\nmetadata:\n  antiq-version: \"{version}\"\n---\n\nOld body\n"
    )
}

fn put_copy(skills_dir: &Path, version: &str) {
    let dir = skills_dir.join("antiq");
    fs::create_dir_all(&dir).expect("a creatable directory");
    fs::write(dir.join("SKILL.md"), installed_skill(version)).expect("a writable file");
}

// =========================================================
// The text of the skill
// =========================================================

#[test]
fn the_skill_has_agent_skills_frontmatter() {
    assert!(
        core().starts_with("---\nname: antiq\ndescription: "),
        "{}",
        core()
    );
}

#[test]
fn the_skill_carries_the_version_of_this_antiq() {
    // Every copy of the skill, installed or saved from `antiq skill`, names
    // the antiq it belongs to. A release that bumps the crate version fails
    // here until the skill names the new one.
    let text = core();
    let frontmatter = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .expect("the skill has frontmatter")
        .0;
    let stamp = format!(
        "\nmetadata:\n  antiq-version: \"{}\"",
        env!("CARGO_PKG_VERSION")
    );

    assert!(frontmatter.contains(&stamp), "{frontmatter}");
}

#[test]
fn the_core_and_every_part_end_with_their_end_line() {
    // An agent that read only part of a file can tell from the missing end
    // line that it has to read on.
    assert!(core().ends_with("\nEnd of the antiq skill.\n"));
    for name in PARTS {
        assert!(
            part(name).ends_with(&format!("\nEnd of references/{name}.md.\n")),
            "{name}"
        );
    }
}

#[test]
fn the_core_names_every_part_by_file_and_by_command() {
    // A host that loads the skill directory follows the file; an agent that
    // only ran `antiq skill` has no file and needs the command.
    let text = core();
    for name in PARTS {
        assert!(text.contains(&format!("references/{name}.md")), "{name}");
        assert!(text.contains(&format!("`antiq skill {name}`")), "{name}");
    }
}

#[test]
fn the_repository_holds_no_part_the_binary_does_not_ship() {
    let mut on_disk: Vec<String> = fs::read_dir(skill_dir().join("references"))
        .expect("the skill has a references directory")
        .map(|entry| {
            entry
                .expect("a readable entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    on_disk.sort_unstable();
    let mut shipped: Vec<String> = PARTS.iter().map(|name| format!("{name}.md")).collect();
    shipped.sort_unstable();

    assert_eq!(on_disk, shipped);
}

// =========================================================
// Printing
// =========================================================

#[test]
fn prints_the_skill_of_this_version() {
    let cwd = empty_dir();
    let run = skill(cwd.path(), &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, core());
    assert_eq!(run.stderr, "");
}

#[test]
fn prints_each_part_of_the_skill() {
    let cwd = empty_dir();
    for name in PARTS {
        let run = skill(cwd.path(), &[name]);

        assert_eq!(run.code(), 0, "{name}: {}", run.stderr);
        assert_eq!(run.stdout, part(name), "{name}");
    }
}

#[test]
fn an_unknown_part_is_a_usage_error_that_names_the_parts() {
    let cwd = empty_dir();
    let run = skill(cwd.path(), &["pushing"]);

    assert_eq!(run.code(), 2, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "");
    for name in PARTS {
        assert!(run.stderr.contains(name), "{name}: {}", run.stderr);
    }
}

#[test]
fn json_carries_the_skill_in_the_envelope() {
    let cwd = empty_dir();
    let run = Antiq::new()
        .current_dir(cwd.path())
        .args(["--json", "skill"])
        .run();

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(
        run.json(),
        json!({
            "format": 1,
            "command": "skill",
            "part": null,
            "skill": core(),
            "warnings": [],
        })
    );
}

#[test]
fn json_carries_a_part_with_its_name() {
    let cwd = empty_dir();
    let run = Antiq::new()
        .current_dir(cwd.path())
        .args(["--json", "skill", "damage"])
        .run();

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.json()["part"], json!("damage"));
    assert_eq!(run.json()["skill"], json!(part("damage")));
}

#[test]
fn an_unknown_part_under_json_is_a_usage_error_of_skill() {
    let cwd = empty_dir();
    let run = Antiq::new()
        .current_dir(cwd.path())
        .args(["--json", "skill", "pushing"])
        .run();

    assert_eq!(run.code(), 2);
    assert_eq!(run.json()["command"], json!("skill"));
    assert_eq!(run.error_kind(), "usage");
}

// =========================================================
// Installing
// =========================================================

/// The files of `dir/antiq`, relative to it, sorted.
fn files_below(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("a readable directory") {
            let path = entry.expect("a readable entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(dir).expect("below the directory");
                found.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    found.sort_unstable();
    found
}

#[test]
fn install_writes_the_core_and_every_part() {
    let cwd = empty_dir();
    let run = skill(cwd.path(), &["--install", ".claude/skills"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let installed = cwd.path().join(".claude/skills/antiq");
    assert_eq!(
        fs::read_to_string(installed.join("SKILL.md")).expect("the core is written"),
        core()
    );
    for name in PARTS {
        assert_eq!(
            fs::read_to_string(installed.join(format!("references/{name}.md")))
                .expect("the part is written"),
            part(name),
            "{name}"
        );
    }
}

#[test]
fn install_names_each_file_it_wrote() {
    let cwd = empty_dir();
    let run = skill(cwd.path(), &["--install", "skills"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let root = fs::canonicalize(cwd.path()).expect("the temp dir exists");
    let mut expected = vec![format!(
        "installed {}",
        root.join("skills/antiq/SKILL.md").display()
    )];
    expected.extend(PARTS.iter().map(|name| {
        format!(
            "installed {}",
            root.join(format!("skills/antiq/references/{name}.md"))
                .display()
        )
    }));
    assert_eq!(run.stdout.lines().collect::<Vec<_>>(), expected);
}

#[test]
fn install_replaces_the_references_and_keeps_other_files() {
    let cwd = empty_dir();
    let installed = cwd.path().join("skills/antiq");
    fs::create_dir_all(installed.join("references")).expect("a creatable directory");
    fs::write(installed.join("references/gone.md"), "an earlier part").expect("writable");
    fs::write(installed.join("notes.txt"), "the user's own").expect("writable");

    let run = skill(cwd.path(), &["--install", "skills"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let mut expected = vec!["SKILL.md".to_string(), "notes.txt".to_string()];
    expected.extend(PARTS.iter().map(|name| format!("references/{name}.md")));
    expected.sort_unstable();
    assert_eq!(files_below(&installed), expected);
}

#[test]
fn install_takes_a_relative_directory_from_dash_c() {
    let cwd = empty_dir();
    let elsewhere = empty_dir();
    let run = Antiq::new()
        .current_dir(cwd.path())
        .arg("-C")
        .arg(elsewhere.path())
        .args(["skill", "--install", "skills"])
        .run();

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(elsewhere.path().join("skills/antiq/SKILL.md").is_file());
    assert!(!cwd.path().join("skills").exists());
}

#[test]
fn install_under_json_lists_the_files_it_wrote() {
    let cwd = empty_dir();
    let run = Antiq::new()
        .current_dir(cwd.path())
        .args(["--json", "skill", "--install", "skills"])
        .run();

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let root = fs::canonicalize(cwd.path()).expect("the temp dir exists");
    let mut installed = vec![Value::from(
        root.join("skills/antiq/SKILL.md").display().to_string(),
    )];
    installed.extend(PARTS.iter().map(|name| {
        Value::from(
            root.join(format!("skills/antiq/references/{name}.md"))
                .display()
                .to_string(),
        )
    }));
    assert_eq!(
        run.json(),
        json!({"format": 1, "command": "skill", "installed": installed})
    );
}

#[test]
fn install_into_a_file_fails_and_names_the_directory() {
    let cwd = empty_dir();
    fs::write(cwd.path().join("skills"), "a file, not a directory").expect("writable");

    let run = Antiq::new()
        .current_dir(cwd.path())
        .args(["--json", "skill", "--install", "skills"])
        .run();

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
    assert!(
        run.error_message().contains("skills/antiq"),
        "{}",
        run.error_message()
    );
}

#[test]
fn a_part_and_install_together_are_a_usage_error() {
    let cwd = empty_dir();
    let run = skill(cwd.path(), &["damage", "--install", "skills"]);

    assert_eq!(run.code(), 2, "stderr: {}", run.stderr);
    assert!(!cwd.path().join("skills").exists());
}

// =========================================================
// An installed copy older than this antiq
// =========================================================

#[test]
fn an_older_project_copy_is_a_warning_after_the_skill() {
    let cwd = empty_dir();
    put_copy(&cwd.path().join(".claude/skills"), "0.9.0");

    let run = skill(cwd.path(), &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, core());
    // A pipe gets the one line of a warning; the fix is in the JSON and in
    // the block a terminal shows.
    assert_eq!(
        run.stderr,
        format!(
            "warning: the antiq skill in .claude/skills/antiq is from antiq 0.9.0, this antiq is {}\n",
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn an_older_copy_is_a_warning_object_under_json() {
    let cwd = empty_dir();
    put_copy(&cwd.path().join(".agents/skills"), "0.9.0");

    let run = Antiq::new()
        .current_dir(cwd.path())
        .args(["--json", "skill", "import"])
        .run();

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "");
    assert_eq!(
        run.json()["warnings"],
        json!([{
            "code": "antiq::skill::outdated",
            "message": format!(
                "the antiq skill in .agents/skills/antiq is from antiq 0.9.0, this antiq is {}",
                env!("CARGO_PKG_VERSION")
            ),
            "fix": "Read the current one with `antiq skill`, and ask the user whether to update the copy with `antiq skill --install .agents/skills`.",
        }])
    );
}

#[test]
fn a_global_copy_under_home_is_named_from_home() {
    let cwd = empty_dir();
    let home = empty_dir();
    put_copy(&home.path().join(".claude/skills"), "0.9.0");

    let run = Antiq::new()
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .arg("skill")
        .run();

    assert!(
        run.stderr
            .contains("the antiq skill in ~/.claude/skills/antiq is from antiq 0.9.0"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_current_copy_gives_no_warning() {
    let cwd = empty_dir();
    put_copy(
        &cwd.path().join(".claude/skills"),
        env!("CARGO_PKG_VERSION"),
    );

    let run = skill(cwd.path(), &[]);

    assert_eq!(run.code(), 0);
    assert_eq!(run.stderr, "");
}

#[test]
fn the_search_starts_in_the_directory_dash_c_names() {
    let cwd = empty_dir();
    let elsewhere = empty_dir();
    put_copy(&elsewhere.path().join(".claude/skills"), "0.9.0");

    let run = Antiq::new()
        .current_dir(cwd.path())
        .arg("-C")
        .arg(elsewhere.path())
        .arg("skill")
        .run();

    assert!(
        run.stderr
            .contains("the antiq skill in .claude/skills/antiq is from antiq 0.9.0"),
        "{}",
        run.stderr
    );
}

#[test]
fn install_gives_no_warning_about_older_copies() {
    let cwd = empty_dir();
    put_copy(&cwd.path().join(".claude/skills"), "0.9.0");

    let run = skill(cwd.path(), &["--install", "elsewhere"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "");
}

#[test]
fn version_warns_about_an_older_copy_after_the_version() {
    let cwd = empty_dir();
    put_copy(&cwd.path().join(".claude/skills"), "0.9.0");

    let run = Antiq::new().current_dir(cwd.path()).arg("--version").run();

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(
        run.stdout,
        concat!("antiq ", env!("CARGO_PKG_VERSION"), "\n")
    );
    assert_eq!(
        run.stderr,
        format!(
            "warning: the antiq skill in .claude/skills/antiq is from antiq 0.9.0, this antiq is {}\n",
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn version_under_json_warns_on_stderr_too() {
    // `--version` prints text also under `--json`, so its warning stays a
    // line on stderr.
    let cwd = empty_dir();
    put_copy(&cwd.path().join(".claude/skills"), "0.9.0");

    let run = Antiq::new()
        .current_dir(cwd.path())
        .args(["--json", "--version"])
        .run();

    assert_eq!(run.code(), 0);
    assert!(
        run.stderr
            .starts_with("warning: the antiq skill in .claude/skills/antiq is from antiq 0.9.0"),
        "{}",
        run.stderr
    );
}

#[test]
fn version_without_an_older_copy_prints_only_the_version() {
    let cwd = empty_dir();

    let run = Antiq::new().current_dir(cwd.path()).arg("--version").run();

    assert_eq!(run.code(), 0);
    assert_eq!(run.stderr, "");
}

#[test]
fn version_in_a_removed_directory_prints_the_version_and_no_warning() {
    // A directory removed under the process leaves nothing to search from.
    let parent = empty_dir();
    let gone = parent.path().join("gone");
    fs::create_dir(&gone).expect("a creatable directory");

    let run = Antiq::program("sh")
        .arg("-c")
        .arg(r#"cd "$1" && rmdir "$1" && exec "$2" --version"#)
        .arg("sh")
        .arg(&gone)
        .arg(env!("CARGO_BIN_EXE_antiq"))
        .run();

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(
        run.stdout,
        concat!("antiq ", env!("CARGO_PKG_VERSION"), "\n")
    );
    assert_eq!(run.stderr, "");
}
