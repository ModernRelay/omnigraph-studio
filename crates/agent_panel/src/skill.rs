//! The `omnigraph-studio` agent skill (spec 013): the OmniGraph guide that matches the CLI
//! contract Studio speaks, with Studio's own entry point, written into a graph's agent working
//! directory where each agent looks for skills. Claude Code reads `.claude/skills` and Codex
//! `.agents/skills`; neither reads the other's.
//!
//! The name differs from the upstream `omnigraph` skill on purpose. With a personal `omnigraph`
//! skill installed, Claude Code hides a project skill of the same name and Codex lists both but
//! loads neither by name.

use std::path::Path;

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

/// Writes the skill under `cwd` for every agent, replacing files that differ and removing files
/// a previous Studio wrote that this one does not. Nothing outside the skill's folders changes.
pub fn write(cwd: &Path) -> std::io::Result<()> {
    for root in ROOTS {
        write_tree(&cwd.join(root).join(NAME))?;
    }
    Ok(())
}

fn write_tree(dir: &Path) -> std::io::Result<()> {
    for (path, text) in FILES {
        let target = dir.join(path);
        if std::fs::read(&target).is_ok_and(|current| current == text.as_bytes()) {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, text)?;
    }
    remove_stale(dir, dir)
}

/// Removes files under `dir` that are not in [`FILES`], without following links.
fn remove_stale(root: &Path, dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let kind = std::fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            remove_stale(root, &path)?;
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let known = FILES.iter().any(|(name, _)| Path::new(name) == relative);
        if !known {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
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

        write(&cwd).unwrap();
        for root in ROOTS {
            let dir = cwd.join(root).join(NAME);
            for (name, text) in FILES {
                assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), *text);
            }
        }
        assert!(!stale.exists());
        assert_eq!(std::fs::read_to_string(&neighbour).unwrap(), "keep");
        // A second write changes nothing.
        write(&cwd).unwrap();
        std::fs::remove_dir_all(&cwd).ok();
    }
}
