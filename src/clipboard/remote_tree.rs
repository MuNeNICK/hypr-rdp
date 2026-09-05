//! The directory tree a client advertises through `FileGroupDescriptorW`.
//!
//! The client sends a flat list of descriptors, each carrying a file name and
//! the relative path of the directory holding it. Rebuilding the tree that list
//! describes is what lets the mount be browsed rather than offering one file.
//!
//! This is also where the list stops being trusted. Every path component is
//! checked, parents the client never sent are synthesized, and both the size of
//! the tree and the work spent building it are bounded.

use std::collections::HashMap;
use std::time::SystemTime;

use ironrdp_cliprdr::pdu::{ClipboardFileAttributes, FileDescriptor};

use super::files::{system_time, MAX_DIRECTORY_DEPTH};

/// The mount root's inode, which the kernel fixes at 1. Kept as a plain integer
/// so the tree stays a data structure with no opinion about the filesystem
/// crate that serves it.
pub(super) const ROOT_INODE: u64 = 1;

/// The first inode a tree may hand out. The root owns everything below it.
pub(super) const FIRST_INODE: u64 = ROOT_INODE + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RemoteNodeKind {
    Directory,
    /// A regular file, at this index in the descriptor list the client sent.
    File {
        index: i32,
    },
}

#[derive(Debug)]
pub(super) struct RemoteNode {
    pub(super) name: String,
    pub(super) parent: u64,
    pub(super) size: u64,
    pub(super) modified: SystemTime,
    pub(super) kind: RemoteNodeKind,
    /// Whether the client marked this entry read-only. Only that marking makes
    /// it read-only here; the mount being read-only does not.
    read_only: bool,
    children: Vec<u64>,
}

impl RemoteNode {
    pub(super) fn is_directory(&self) -> bool {
        matches!(self.kind, RemoteNodeKind::Directory)
    }

    /// The permission bits the mount reports for this entry, matching what a
    /// GNOME session reports for the same descriptor.
    ///
    /// A file manager pasting out of the mount copies the source mode, so a
    /// mount that reported its own read-only-ness per file would land every
    /// pasted file unwritable by the user who pasted it. The client's
    /// read-only attribute is the only thing that makes an entry read-only;
    /// the kernel enforces the mount's read-only-ness on its own.
    pub(super) fn permissions(&self) -> u16 {
        match (self.is_directory(), self.read_only) {
            (true, false) => 0o755,
            (true, true) => 0o555,
            (false, false) => 0o644,
            (false, true) => 0o444,
        }
    }
}

/// What one client descriptor says about the entry it names, or what a
/// synthesized parent directory carries in the absence of a descriptor.
#[derive(Clone, Copy)]
struct RemoteEntry {
    kind: RemoteNodeKind,
    size: u64,
    modified: Option<SystemTime>,
    read_only: bool,
}

impl RemoteEntry {
    /// A directory the client named only as some entry's parent.
    fn synthesized_directory() -> Self {
        Self {
            kind: RemoteNodeKind::Directory,
            size: 0,
            modified: None,
            read_only: false,
        }
    }
}

/// A browsable tree rebuilt from one client file list.
#[derive(Debug)]
pub(super) struct RemoteTree {
    /// Inode of `nodes[1]`. Each list starts where the previous one ended, so
    /// an inode is never reused for a different file: a handle held open across
    /// a second client copy fails instead of reading another file's bytes.
    base: u64,
    /// `nodes[0]` is always the root.
    nodes: Vec<RemoteNode>,
    /// Name lookup, so neither the build nor the mount scans a directory's
    /// children. A client may send tens of thousands of entries into one
    /// directory, and both paths run where a linear scan would be felt.
    by_name: HashMap<(u64, String), u64>,
    /// Stands in as the modification time of anything the client did not stamp.
    built_at: SystemTime,
}

impl RemoteTree {
    pub(super) fn empty() -> Self {
        Self::rooted(FIRST_INODE)
    }

    fn rooted(base: u64) -> Self {
        let built_at = SystemTime::now();
        Self {
            base,
            nodes: vec![RemoteNode {
                name: String::new(),
                parent: ROOT_INODE,
                size: 0,
                modified: built_at,
                kind: RemoteNodeKind::Directory,
                // The mount point itself is nobody's to write.
                read_only: true,
                children: Vec::new(),
            }],
            by_name: HashMap::new(),
            built_at,
        }
    }

    /// Rebuilds the tree described by `files`, dropping entries whose names
    /// cannot be trusted and stopping at `max_entries` nodes. `base` is the
    /// first inode to hand out; see [`RemoteTree::next_base`].
    pub(super) fn build(files: &[FileDescriptor], max_entries: usize, base: u64) -> Self {
        let mut tree = Self::rooted(base);
        let mut dropped = 0usize;

        for (index, file) in files.iter().enumerate() {
            // Nothing further can be inserted, so stop rather than walk the
            // rest of a list a hostile client made as long as the wire allows.
            if tree.is_full(max_entries) {
                dropped += files.len() - index;
                break;
            }
            let Ok(index) = i32::try_from(index) else {
                dropped += files.len() - index;
                break;
            };
            let Some(components) = path_components(file) else {
                tracing::warn!(name = %file.name, "Clipboard: dropping unusable remote file name");
                dropped += 1;
                continue;
            };
            let Some((leaf, parents)) = components.split_last() else {
                continue;
            };

            let mut parent = ROOT_INODE;
            let mut reached = true;
            for component in parents {
                match tree.directory(parent, component, max_entries) {
                    Some(inode) => parent = inode,
                    None => {
                        reached = false;
                        break;
                    }
                }
            }
            if !reached {
                dropped += 1;
                continue;
            }

            let entry = RemoteEntry {
                kind: if is_directory(file) {
                    RemoteNodeKind::Directory
                } else {
                    RemoteNodeKind::File { index }
                },
                size: file_size(file),
                modified: file.last_write_time.and_then(system_time),
                read_only: is_read_only(file),
            };
            if tree.insert(parent, leaf, entry, max_entries).is_none() {
                dropped += 1;
            }
        }

        if dropped > 0 {
            tracing::warn!(
                dropped,
                max_entries,
                "Clipboard: remote file list did not fit the inbound tree"
            );
        }
        tree
    }

    /// The first inode the next tree may hand out.
    pub(super) fn next_base(&self) -> u64 {
        self.base.saturating_add(self.entries() as u64)
    }

    pub(super) fn node(&self, inode: u64) -> Option<&RemoteNode> {
        if inode == ROOT_INODE {
            return self.nodes.first();
        }
        let offset = usize::try_from(inode.checked_sub(self.base)?).ok()?;
        self.nodes.get(offset.checked_add(1)?)
    }

    pub(super) fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        // Borrowing the key would need a custom Borrow impl for the pair; the
        // clone is one short name per path component resolved.
        self.by_name.get(&(parent, name.to_owned())).copied()
    }

    pub(super) fn children(&self, inode: u64) -> &[u64] {
        self.node(inode)
            .map_or(&[], |node| node.children.as_slice())
    }

    /// The entries the client put on its clipboard, which are what the Wayland
    /// side advertises as URIs.
    pub(super) fn roots(&self) -> &[u64] {
        self.children(ROOT_INODE)
    }

    /// A directory's `st_nlink`: itself, its parent's entry for it, and one per
    /// subdirectory. Tools that walk with `fts` — `find`, `du`, `rm -r` — read
    /// this as the subdirectory count and stop descending if it says zero.
    pub(super) fn link_count(&self, inode: u64) -> u32 {
        let Some(node) = self.node(inode) else {
            return 1;
        };
        if !node.is_directory() {
            return 1;
        }
        let subdirectories = self
            .children(inode)
            .iter()
            .filter(|child| self.node(**child).is_some_and(RemoteNode::is_directory))
            .count();
        2u32.saturating_add(u32::try_from(subdirectories).unwrap_or(u32::MAX))
    }

    /// Walks a `/`-separated path from the root the way the mount's lookup
    /// does, so a test can assert on the tree a file manager would browse.
    #[cfg(test)]
    pub(super) fn resolve(&self, path: &str) -> Option<u64> {
        let mut inode = ROOT_INODE;
        for component in path.split('/') {
            inode = self.lookup(inode, component)?;
        }
        Some(inode)
    }

    /// Entries below the root. The root is the mount point, not an entry.
    fn entries(&self) -> usize {
        self.nodes.len() - 1
    }

    fn is_full(&self, max_entries: usize) -> bool {
        self.entries() >= max_entries
    }

    /// Resolves one path component to a directory, creating it when the client
    /// listed a file before the directory holding it — or never listed it.
    fn directory(&mut self, parent: u64, name: &str, max_entries: usize) -> Option<u64> {
        match self.lookup(parent, name) {
            Some(inode) if self.node(inode).is_some_and(RemoteNode::is_directory) => Some(inode),
            Some(_) => None,
            None => self.insert(
                parent,
                name,
                RemoteEntry::synthesized_directory(),
                max_entries,
            ),
        }
    }

    /// Adds one node under `parent`. A repeated directory is the same
    /// directory; any other repeated name is a malformed list, and the entry
    /// that arrived first keeps the name.
    fn insert(
        &mut self,
        parent: u64,
        name: &str,
        entry: RemoteEntry,
        max_entries: usize,
    ) -> Option<u64> {
        if let Some(inode) = self.lookup(parent, name) {
            if !self.node(inode)?.is_directory() || entry.kind != RemoteNodeKind::Directory {
                return None;
            }
            // A directory synthesized for a child that arrived first carries no
            // metadata of its own until the client describes it.
            let offset = usize::try_from(inode.checked_sub(self.base)?).ok()? + 1;
            if let Some(modified) = entry.modified {
                self.nodes.get_mut(offset)?.modified = modified;
            }
            if entry.read_only {
                self.nodes.get_mut(offset)?.read_only = true;
            }
            return Some(inode);
        }
        if self.is_full(max_entries) {
            return None;
        }

        let inode = self.base.checked_add(self.entries() as u64)?;
        self.nodes.push(RemoteNode {
            name: name.to_owned(),
            parent,
            size: entry.size,
            modified: entry.modified.unwrap_or(self.built_at),
            kind: entry.kind,
            read_only: entry.read_only,
            children: Vec::new(),
        });
        self.by_name.insert((parent, name.to_owned()), inode);
        let parent_offset = if parent == ROOT_INODE {
            0
        } else {
            usize::try_from(parent.checked_sub(self.base)?).ok()? + 1
        };
        self.nodes.get_mut(parent_offset)?.children.push(inode);
        Some(inode)
    }
}

/// Splits one descriptor into the path components it names, or `None` when the
/// client sent something that has no place in the tree.
///
/// Both separators are split — including inside the name, which the client is
/// not supposed to put there — so that a name carrying a path nests instead of
/// reaching outside the mount.
fn path_components(file: &FileDescriptor) -> Option<Vec<&str>> {
    let components: Vec<&str> = file
        .relative_path
        .as_deref()
        .unwrap_or_default()
        .split(['\\', '/'])
        .chain(file.name.split(['\\', '/']))
        .filter(|component| !component.is_empty())
        .collect();

    // The leaf's depth, counted the way the outbound walk counts it.
    let depth = components.len().saturating_sub(1);
    let usable = !components.is_empty()
        && depth <= MAX_DIRECTORY_DEPTH
        && components
            .iter()
            .all(|component| !matches!(*component, "." | "..") && !component.contains('\0'));
    usable.then_some(components)
}

fn is_directory(file: &FileDescriptor) -> bool {
    file.attributes
        .is_some_and(|attributes| attributes.contains(ClipboardFileAttributes::DIRECTORY))
}

fn is_read_only(file: &FileDescriptor) -> bool {
    file.attributes
        .is_some_and(|attributes| attributes.contains(ClipboardFileAttributes::READONLY))
}

fn file_size(file: &FileDescriptor) -> u64 {
    if is_directory(file) {
        0
    } else {
        file.file_size.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use super::super::files::WINDOWS_EPOCH_OFFSET_SECS;

    fn directory(name: &str, path: Option<&str>) -> FileDescriptor {
        let descriptor =
            FileDescriptor::new(name).with_attributes(ClipboardFileAttributes::DIRECTORY);
        match path {
            Some(path) => descriptor.with_relative_path(path),
            None => descriptor,
        }
    }

    fn file(name: &str, path: Option<&str>, size: u64) -> FileDescriptor {
        let descriptor = FileDescriptor::new(name)
            .with_attributes(ClipboardFileAttributes::NORMAL)
            .with_file_size(size);
        match path {
            Some(path) => descriptor.with_relative_path(path),
            None => descriptor,
        }
    }

    fn build(files: &[FileDescriptor], max_entries: usize) -> RemoteTree {
        RemoteTree::build(files, max_entries, FIRST_INODE)
    }

    fn at<'a>(tree: &'a RemoteTree, path: &str) -> Option<(u64, &'a RemoteNode)> {
        let inode = tree.resolve(path)?;
        Some((inode, tree.node(inode)?))
    }

    fn names(tree: &RemoteTree, inode: u64) -> Vec<&str> {
        tree.children(inode)
            .iter()
            .map(|child| tree.node(*child).expect("child exists").name.as_str())
            .collect()
    }

    #[test]
    fn rebuilds_a_nested_tree_with_its_empty_directories() {
        let tree = build(
            &[
                directory("project", None),
                directory("src", Some("project")),
                file("main.rs", Some("project\\src"), 12),
                directory("empty", Some("project")),
            ],
            100,
        );

        assert_eq!(names(&tree, ROOT_INODE), ["project"]);
        let (project, _) = at(&tree, "project").expect("project exists");
        assert_eq!(names(&tree, project), ["src", "empty"]);
        let (empty, node) = at(&tree, "project/empty").expect("empty directory exists");
        assert!(node.is_directory());
        assert!(tree.children(empty).is_empty());
        let (_, main) = at(&tree, "project/src/main.rs").expect("nested file exists");
        assert_eq!(main.kind, RemoteNodeKind::File { index: 2 });
        assert_eq!(main.size, 12);
    }

    /// `fts`-based tools read a directory's link count as its subdirectory
    /// count and stop descending when it says there are none.
    #[test]
    fn a_directory_links_itself_its_parent_and_each_subdirectory() {
        let tree = build(
            &[
                directory("project", None),
                directory("src", Some("project")),
                directory("empty", Some("project")),
                file("main.rs", Some("project\\src"), 1),
            ],
            100,
        );

        let (project, _) = at(&tree, "project").expect("project exists");
        assert_eq!(tree.link_count(project), 4);
        let (src, _) = at(&tree, "project/src").expect("src exists");
        assert_eq!(tree.link_count(src), 2);
        let (main, _) = at(&tree, "project/src/main.rs").expect("file exists");
        assert_eq!(tree.link_count(main), 1);
        assert_eq!(tree.link_count(ROOT_INODE), 3);
    }

    #[test]
    fn synthesizes_parent_directories_the_client_never_sent() {
        let tree = build(&[file("deep.txt", Some("a\\b\\c"), 1)], 100);

        let (_, a) = at(&tree, "a").expect("synthesized parent exists");
        assert!(a.is_directory());
        let (_, deep) = at(&tree, "a/b/c/deep.txt").expect("nested file exists");
        assert_eq!(deep.kind, RemoteNodeKind::File { index: 0 });
    }

    #[test]
    fn nests_separators_in_a_name_instead_of_escaping_the_mount() {
        let tree = build(
            &[
                file("sub/held.txt", None, 1),
                file("..\\escape.txt", None, 1),
                file("\\\\etc\\passwd", None, 1),
            ],
            100,
        );

        assert_eq!(names(&tree, ROOT_INODE), ["sub", "etc"]);
        assert!(at(&tree, "sub/held.txt").is_some());
        assert!(at(&tree, "etc/passwd").is_some());
    }

    #[test]
    fn drops_entries_beyond_the_entry_and_depth_ceilings() {
        let deep = vec!["d"; MAX_DIRECTORY_DEPTH + 1].join("\\");
        let tree = build(
            &[
                file("too-deep.txt", Some(&deep), 1),
                file("kept.txt", None, 1),
            ],
            100,
        );

        assert_eq!(names(&tree, ROOT_INODE), ["kept.txt"]);

        let many: Vec<FileDescriptor> = (0..10)
            .map(|index| file(&format!("{index}.txt"), None, 1))
            .collect();
        let bounded = build(&many, 4);

        assert_eq!(bounded.roots().len(), 4);
    }

    #[test]
    fn merges_repeated_directories_and_keeps_the_first_file_of_a_name() {
        let tree = build(
            &[
                directory("shared", None),
                file("one.txt", Some("shared"), 1),
                directory("shared", None),
                file("two.txt", Some("shared"), 2),
                file("one.txt", Some("shared"), 9),
            ],
            100,
        );

        assert_eq!(names(&tree, ROOT_INODE), ["shared"]);
        let (shared, _) = at(&tree, "shared").expect("shared directory exists");
        assert_eq!(names(&tree, shared), ["one.txt", "two.txt"]);
        let (_, one) = at(&tree, "shared/one.txt").expect("first file wins");
        assert_eq!(one.kind, RemoteNodeKind::File { index: 1 });
    }

    #[test]
    fn carries_the_client_modification_time_onto_the_node() {
        let stamp = |seconds: u64| (WINDOWS_EPOCH_OFFSET_SECS + seconds) * 10_000_000;
        let before = SystemTime::now();
        let tree = build(
            &[
                file("held/stamped.txt", None, 1).with_last_write_time(stamp(1_000)),
                directory("held", None).with_last_write_time(stamp(2_000)),
                file("unstamped.txt", None, 1),
            ],
            100,
        );

        let (_, node) = at(&tree, "held/stamped.txt").expect("file exists");
        assert_eq!(
            node.modified,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)
        );
        // The directory was synthesized for its child, then described.
        let (_, held) = at(&tree, "held").expect("directory exists");
        assert_eq!(
            held.modified,
            SystemTime::UNIX_EPOCH + Duration::from_secs(2_000)
        );
        // A descriptor the client did not stamp is dated to the paste, not 1970.
        let (_, unstamped) = at(&tree, "unstamped.txt").expect("file exists");
        assert!(unstamped.modified >= before);
    }

    /// A second copy on the client must not hand an inode that a file manager
    /// still holds open to a different file.
    #[test]
    fn a_later_list_never_reuses_an_earlier_inode() {
        let first = build(
            &[file("first.txt", None, 1), file("second.txt", None, 1)],
            100,
        );
        let held = first.resolve("first.txt").expect("file exists");

        let second = RemoteTree::build(&[file("other.txt", None, 1)], 100, first.next_base());

        assert!(second.node(held).is_none());
        assert_eq!(second.roots().len(), 1);
        assert_ne!(second.resolve("other.txt"), Some(held));
    }

    /// A pasted file is copied with the mode the mount reports, so reporting
    /// the mount's own read-only-ness per file would leave every pasted file
    /// unwritable by the user who pasted it.
    #[test]
    fn an_ordinary_entry_arrives_writable_by_its_owner() {
        let tree = build(
            &[
                directory("project", None),
                file("main.rs", Some("project"), 4),
            ],
            100,
        );

        let (_, project) = at(&tree, "project").expect("directory exists");
        let (_, main) = at(&tree, "project/main.rs").expect("file exists");

        assert_eq!(project.permissions(), 0o755);
        assert_eq!(main.permissions(), 0o644);
    }

    #[test]
    fn only_an_entry_the_client_marked_read_only_arrives_read_only() {
        let read_only = |descriptor: FileDescriptor, attributes| {
            descriptor.with_attributes(attributes | ClipboardFileAttributes::READONLY)
        };
        let tree = build(
            &[
                read_only(
                    directory("locked", None),
                    ClipboardFileAttributes::DIRECTORY,
                ),
                read_only(
                    file("notes.txt", Some("locked"), 4),
                    ClipboardFileAttributes::NORMAL,
                ),
            ],
            100,
        );

        let (_, locked) = at(&tree, "locked").expect("directory exists");
        let (_, notes) = at(&tree, "locked/notes.txt").expect("file exists");

        assert_eq!(locked.permissions(), 0o555);
        assert_eq!(notes.permissions(), 0o444);
    }

    /// A directory the client named only as a parent carries no attributes of
    /// its own, and picks them up when the client finally describes it.
    #[test]
    fn a_synthesized_directory_is_writable_until_the_client_marks_it_read_only() {
        let tree = build(&[file("deep.txt", Some("outer"), 1)], 100);
        let (_, synthesized) = at(&tree, "outer").expect("directory exists");
        assert_eq!(synthesized.permissions(), 0o755);

        let tree = build(
            &[
                file("deep.txt", Some("outer"), 1),
                directory("outer", None).with_attributes(
                    ClipboardFileAttributes::DIRECTORY | ClipboardFileAttributes::READONLY,
                ),
            ],
            100,
        );
        let (_, described) = at(&tree, "outer").expect("directory exists");
        assert_eq!(described.permissions(), 0o555);
    }

    #[test]
    fn the_mount_point_itself_is_never_writable() {
        let tree = build(&[file("one.txt", None, 1)], 100);

        assert_eq!(
            tree.node(ROOT_INODE).expect("root exists").permissions(),
            0o555
        );
    }
}
