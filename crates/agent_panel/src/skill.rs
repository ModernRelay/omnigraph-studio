//! The `omnigraph-studio` agent skill (spec 013): the Omnigraph guide that matches the CLI
//! contract Studio speaks, with Studio's own entry point, written into a graph's agent working
//! directory where each agent looks for skills. Claude Code reads `.claude/skills` and Codex
//! `.agents/skills`; neither reads the other's.
//!
//! The name differs from the upstream `omnigraph` skill on purpose. With a personal `omnigraph`
//! skill installed, Claude Code hides a project skill of the same name and Codex lists both but
//! loads neither by name.

use std::path::Path;

use crate::confined::{Confined, Kind};

/// The skill's folder name, which matches the `name` in its `SKILL.md`.
pub const NAME: &str = "omnigraph-studio";

/// Where agents look for project skills, relative to the working directory.
const ROOTS: &[&str] = &[".claude/skills", ".agents/skills"];

/// Every file of the skill, by its path inside the skill's folder. The upstream entry point is
/// written as `GUIDE.md` so agents do not list it as a skill of its own.
const FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../skill/SKILL.md")),
    ("GUIDE.md", include_str!("../skill/upstream/SKILL.md")),
    ("LICENSE", include_str!("../skill/upstream/LICENSE")),
    (
        "references/aliases.md",
        include_str!("../skill/upstream/references/aliases.md"),
    ),
    (
        "references/blobs.md",
        include_str!("../skill/upstream/references/blobs.md"),
    ),
    (
        "references/changes.md",
        include_str!("../skill/upstream/references/changes.md"),
    ),
    (
        "references/cluster.md",
        include_str!("../skill/upstream/references/cluster.md"),
    ),
    (
        "references/commands.md",
        include_str!("../skill/upstream/references/commands.md"),
    ),
    (
        "references/data.md",
        include_str!("../skill/upstream/references/data.md"),
    ),
    (
        "references/migrations.md",
        include_str!("../skill/upstream/references/migrations.md"),
    ),
    (
        "references/queries.md",
        include_str!("../skill/upstream/references/queries.md"),
    ),
    (
        "references/remote-ops.md",
        include_str!("../skill/upstream/references/remote-ops.md"),
    ),
    (
        "references/schema-design.md",
        include_str!("../skill/upstream/references/schema-design.md"),
    ),
    (
        "references/schema.md",
        include_str!("../skill/upstream/references/schema.md"),
    ),
    (
        "references/search.md",
        include_str!("../skill/upstream/references/search.md"),
    ),
    (
        "references/server-policy.md",
        include_str!("../skill/upstream/references/server-policy.md"),
    ),
    (
        "references/stored-queries.md",
        include_str!("../skill/upstream/references/stored-queries.md"),
    ),
];

/// Writes the skill under `cwd` for every agent, replacing files that differ and removing, in the
/// folders the skill ships, every file this Studio does not (one a previous Studio wrote, or an
/// agent left there). Nothing outside the skill's folders changes: the folders are opened by
/// handle, never through a link the agent left (`confined`), and another folder is not walked.
pub fn write(cwd: &Path) -> std::io::Result<()> {
    let cwd = Confined::open(cwd)?;
    // Each agent's folder on its own: one that cannot be written (an agent left a link there)
    // does not keep the other agent's skill from being written. The first failure is said.
    let mut failed = None;
    for root in ROOTS {
        let written = cwd
            .dirs(root)
            .and_then(|dirs| dirs.dir(NAME))
            .and_then(|dir| write_tree(&dir));
        if let Err(error) = written {
            failed.get_or_insert(error);
        }
    }
    failed.map_or(Ok(()), Err)
}

fn write_tree(dir: &Confined) -> std::io::Result<()> {
    for (path, text) in FILES {
        let nested;
        let (folder, name) = match path.rsplit_once('/') {
            Some((parent, name)) => {
                nested = dir.dirs(parent)?;
                (&nested, name)
            }
            None => (dir, *path),
        };
        if folder.holds(name, text.as_bytes()) {
            continue;
        }
        folder.write(name, text.as_bytes(), 0o644)?;
    }
    remove_stale(dir, "")
}

/// Removes files under `dir` that are not in [`FILES`]; `prefix` is `dir`'s path in the skill.
/// Only a folder [`FILES`] holds files in is walked: another (an agent's, of any depth) holds
/// nothing Studio wrote, and is left as it is, so walking costs what the skill ships.
fn remove_stale(dir: &Confined, prefix: &str) -> std::io::Result<()> {
    for (name, kind) in dir.entries()? {
        let relative = match prefix {
            "" => name.to_string_lossy().into_owned(),
            prefix => format!("{prefix}/{}", name.to_string_lossy()),
        };
        match kind {
            Kind::Dir => {
                if let Some(folder) = name.to_str()
                    && ships_folder(&relative)
                {
                    remove_stale(&dir.dir(folder)?, &relative)?;
                }
            }
            Kind::Other if FILES.iter().any(|(known, _)| *known == relative) => {}
            Kind::Other => dir.remove(&name)?,
        }
    }
    Ok(())
}

/// Whether the skill ships a file in the folder at `relative` (`references`).
fn ships_folder(relative: &str) -> bool {
    FILES.iter().any(|(path, _)| {
        path.strip_prefix(relative)
            .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_upstream_file_ships_and_the_entry_point_names_the_skill() {
        let upstream = Path::new(env!("CARGO_MANIFEST_DIR")).join("skill/upstream");
        let mut on_disk = Vec::new();
        let mut stack = vec![upstream.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let relative = path.strip_prefix(&upstream).unwrap().to_owned();
                    on_disk.push(if relative == Path::new("SKILL.md") {
                        "GUIDE.md".to_owned()
                    } else {
                        // FILES names paths with `/`, as the skill's folders do everywhere.
                        relative
                            .components()
                            .map(|part| part.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/")
                    });
                }
            }
        }
        on_disk.sort();
        let mut shipped: Vec<String> = FILES
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .filter(|name| name != "SKILL.md")
            .collect();
        shipped.sort();
        assert_eq!(shipped, on_disk, "skill/upstream and FILES differ");

        let entry = FILES[0].1;
        assert!(
            entry.starts_with(&format!("---\nname: {NAME}\n")),
            "the entry point's name must be the folder name"
        );
    }

    /// The guide teaches the operator configuration the agent's CLI reads, Omnigraph's
    /// version-1 file, and none of the target-only format before it, which that CLI refuses.
    #[test]
    fn the_guide_teaches_the_operator_configuration_the_agents_cli_reads() {
        for (name, text) in FILES {
            for retired in [
                "target_config_version",
                "OMNIGRAPH_TOKEN_",
                "credential get",
            ] {
                assert!(!text.contains(retired), "{name} teaches `{retired}`");
            }
        }
        let guide = FILES
            .iter()
            .find(|(name, _)| *name == "GUIDE.md")
            .unwrap()
            .1;
        assert!(guide.contains("`version: 1` document"));
    }

    #[test]
    fn writes_the_skill_for_each_agent_and_removes_only_its_own_stale_files() {
        let cwd = std::env::temp_dir().join(format!("studio-skill-{}", std::process::id()));
        std::fs::remove_dir_all(&cwd).ok();
        // Another skill beside it, and a stale file of an older Studio inside it.
        let neighbour = cwd.join(".claude/skills/other/SKILL.md");
        std::fs::create_dir_all(neighbour.parent().unwrap()).unwrap();
        std::fs::write(&neighbour, "keep").unwrap();
        let stale = cwd
            .join(".agents/skills")
            .join(NAME)
            .join("references/old.md");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "old").unwrap();
        // A folder an agent made inside the skill's, deep: not walked, so left as it is.
        let deep = cwd
            .join(".agents/skills")
            .join(NAME)
            .join("references/notes/a/a/a/a/a/a/a/a");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("mine.md"), "mine").unwrap();

        write(&cwd).unwrap();
        for root in ROOTS {
            let dir = cwd.join(root).join(NAME);
            for (name, text) in FILES {
                assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), *text);
            }
        }
        assert!(!stale.exists());
        assert_eq!(std::fs::read_to_string(&neighbour).unwrap(), "keep");
        assert_eq!(
            std::fs::read_to_string(deep.join("mine.md")).unwrap(),
            "mine",
            "a folder the skill does not ship is not walked"
        );
        // A second write changes nothing.
        write(&cwd).unwrap();
        std::fs::remove_dir_all(&cwd).ok();
    }

    /// A link to a folder elsewhere left inside the skill's folder (a symbolic link, or on
    /// Windows a junction, which any user may make) is removed as a stale entry, and what it
    /// points at is not touched: later writes of the skill go on.
    #[test]
    fn a_link_left_in_the_skill_folder_is_removed_and_not_followed() {
        let cwd = std::env::temp_dir().join(format!("studio-skill-inner-{}", std::process::id()));
        let elsewhere =
            std::env::temp_dir().join(format!("studio-skill-target-{}", std::process::id()));
        for dir in [&cwd, &elsewhere] {
            std::fs::remove_dir_all(dir).ok();
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(elsewhere.join("diary.md"), "mine").unwrap();
        write(&cwd).unwrap();
        // One part at a time: `cmd` takes a `/` in a path for a switch.
        let linked = cwd
            .join(".agents")
            .join("skills")
            .join(NAME)
            .join("references")
            .join("linked");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        #[cfg(windows)]
        {
            let made = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(&linked)
                .arg(&elsewhere)
                .output()
                .unwrap();
            assert!(made.status.success(), "{made:?}");
        }

        write(&cwd).unwrap();
        assert!(
            std::fs::symlink_metadata(&linked).is_err(),
            "the link stays"
        );
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("diary.md")).unwrap(),
            "mine"
        );
        write(&cwd).unwrap();
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
    }

    /// An agent can turn the skill's folder into a link to anywhere, its home included:
    /// Studio refuses to write through it, and removes nothing there.
    #[cfg(unix)]
    #[test]
    fn a_skill_folder_linked_elsewhere_is_not_written_through() {
        let cwd = std::env::temp_dir().join(format!("studio-skill-link-{}", std::process::id()));
        let elsewhere =
            std::env::temp_dir().join(format!("studio-skill-elsewhere-{}", std::process::id()));
        for dir in [&cwd, &elsewhere] {
            std::fs::remove_dir_all(dir).ok();
        }
        std::fs::create_dir_all(cwd.join(".claude/skills")).unwrap();
        std::fs::create_dir_all(elsewhere.join("notes")).unwrap();
        std::fs::write(elsewhere.join("notes/diary.md"), "mine").unwrap();
        std::os::unix::fs::symlink(&elsewhere, cwd.join(".claude/skills").join(NAME)).unwrap();

        assert!(write(&cwd).is_err());
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("notes/diary.md")).unwrap(),
            "mine"
        );
        assert!(!elsewhere.join("SKILL.md").exists());
        // A link higher up is refused the same way.
        std::fs::remove_file(cwd.join(".claude/skills").join(NAME)).unwrap();
        std::fs::remove_dir_all(cwd.join(".claude")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, cwd.join(".claude")).unwrap();
        assert!(write(&cwd).is_err());
        assert!(!elsewhere.join("skills").exists());
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
    }
}
