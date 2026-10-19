use anyhow::Context;
use ini::Ini;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// An XDG desktop entry.
#[derive(Debug, Clone)]
#[expect(unused)]
pub struct DesktopEntry {
    /// The original path at which this desktop entry is located.
    pub source_path: PathBuf,
    /// The path of this desktop entry relative to the `applications` directory it was found in.
    /// This is what identifies the entry: an entry in a more important data directory with the
    /// same relative path overrides this one.
    pub relative_path: PathBuf,
    /// The hash of the desktop entry's content
    pub source_hash: u64,
    pub entry_type: ApplicationType,
    pub name: String,
    pub exec: Option<String>,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    pub icon: Option<String>,
    pub no_display: Option<bool>,
    /// Additional application actions, from `[Desktop Action <id>]` groups.
    pub actions: Vec<DesktopAction>,
}

/// An additional application action, as listed in the `Actions` key of a desktop entry.
#[derive(Debug, Clone)]
pub struct DesktopAction {
    pub name: String,
    pub exec: Option<String>,
}

#[derive(Copy, Clone, Debug, strum::EnumString)]
pub enum ApplicationType {
    Application,
    Link,
    Directory,
}

impl DesktopEntry {}

/// The `applications` directory in `$XDG_DATA_HOME`, where user-specific desktop entries
/// (and overrides of system-wide ones) live.
pub fn user_applications_dir() -> Option<PathBuf> {
    xdg::BaseDirectories::new()
        .data_home
        .map(|data_home| data_home.join("applications"))
}

/// All `applications` directories, from most to least important.
fn applications_dirs() -> Vec<PathBuf> {
    let base_dirs = xdg::BaseDirectories::new();

    let mut data_dirs = base_dirs.data_dirs;
    if let Some(data_home) = base_dirs.data_home {
        data_dirs.insert(0, data_home);
    }

    for dir in &mut data_dirs {
        dir.push("applications");
    }

    data_dirs
}

/// Find the path of a desktop entry by its desktop file ID, e.g. `org.gnome.gedit.desktop`.
pub fn find_desktop_entry_path(id: &str) -> Option<PathBuf> {
    applications_dirs().into_iter().find_map(|dir| {
        // a `-` in the ID may correspond to a subdirectory separator
        [dir.join(id), dir.join(id.replacen('-', "/", 1))]
            .into_iter()
            .find(|path| path.is_file())
    })
}

pub fn load(path: impl AsRef<Path>, relative_path: impl AsRef<Path>) -> anyhow::Result<DesktopEntry> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)?;
    let hash = {
        let mut hasher = std::hash::DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    };

    let ini = Ini::load_from_str(&content)?;
    let main_section = ini
        .section(Some("Desktop Entry"))
        .context("desktop entry does not have a Desktop Entry section")?;

    let entry_type = main_section
        .get("Type")
        .context("desktop entry does not have a Type section")?
        .try_into()?;
    let name = main_section
        .get("Name")
        .context("desktop entry does not have a Name section")?;
    let generic_name = main_section.get("GenericName");
    let comment = main_section.get("Comment");
    let exec = main_section.get("Exec");
    let icon = main_section.get("Icon");
    let no_display = main_section.get("NoDisplay").and_then(|s| s.parse().ok());

    let actions = main_section
        .get("Actions")
        .unwrap_or_default()
        .split(';')
        .filter(|id| !id.is_empty())
        .filter_map(|id| {
            let section = ini.section(Some(format!("Desktop Action {id}")))?;
            Some(DesktopAction {
                name: section.get("Name")?.to_string(),
                exec: section.get("Exec").map(|s| s.to_string()),
            })
        })
        .collect();

    Ok(DesktopEntry {
        source_path: path.to_path_buf(),
        relative_path: relative_path.as_ref().to_path_buf(),
        source_hash: hash,
        entry_type,
        name: name.to_string(),
        exec: exec.map(|s| s.to_string()),
        generic_name: generic_name.map(|s| s.to_string()),
        comment: comment.map(|s| s.to_string()),
        icon: icon.map(|s| s.to_string()),
        no_display,
        actions,
    })
}

struct DesktopEntryIdentifier<'a> {
    base_dir: &'a Path,
    entry: walkdir::DirEntry,
}

impl DesktopEntryIdentifier<'_> {
    fn relative_dir(&self) -> Option<&Path> {
        self.entry.path().strip_prefix(self.base_dir).ok()
    }
}

fn find_desktop_entries_in_base_dir(
    base_dir: &Path,
) -> impl Iterator<Item = DesktopEntryIdentifier<'_>> {
    walkdir::WalkDir::new(base_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "desktop")
                .unwrap_or(false)
        })
        .map(|e| DesktopEntryIdentifier { base_dir, entry: e })
}

pub fn find_desktop_entries() -> Vec<DesktopEntry> {
    let data_dirs = applications_dirs();

    // remove duplicate entries: data dirs are ordered by importance,
    // so the first entry with a given relative path wins.
    let mut seen = std::collections::HashSet::new();

    let identifiers = crate::timing::time("walk applications dirs", || {
        data_dirs
            .iter()
            .flat_map(|dd| find_desktop_entries_in_base_dir(dd))
            .filter_map(|e| {
                let relative = e.relative_dir()?.to_owned();
                seen.insert(relative.clone()).then_some((e, relative))
            })
            .collect::<Vec<_>>()
    });

    // Parsing is the slow part (mostly due to translations), and every entry is independent,
    // so spread it out over a few threads.
    crate::timing::time("parse desktop entries", || {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
        let chunk_size = identifiers.len().div_ceil(threads).max(1);

        std::thread::scope(|scope| {
            let handles: Vec<_> = identifiers
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .filter_map(|(e, relative)| load(e.entry.path(), relative).ok())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();

            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap_or_default())
                .collect()
        })
    })
}
