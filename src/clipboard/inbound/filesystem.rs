//! FUSE replies for one immutable selection generation.
use super::transfer::Transfer;
use super::tree::{RemoteNode, RemoteNodeKind, RemoteTree};
use std::time::Duration;

/// The mount's view of the advertised tree.
///
/// Deliberately thin: every handler is a tree lookup, and a read translates an
/// inode into the same index-and-range request the backend seam already
/// exercises without a mount.
#[derive(Clone)]
pub(super) struct RemoteFilesystem {
    files: Transfer,
    generation: u64,
}

impl RemoteFilesystem {
    pub(super) fn new(files: Transfer, generation: u64) -> Self {
        Self { files, generation }
    }
}

impl fuser::Filesystem for RemoteFilesystem {
    fn getattr(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: fuser::ReplyAttr,
    ) {
        let files = self.files.clone();
        let generation = self.generation;
        self.files.runtime.spawn(async move {
            if files.size(generation, ino.0).await.is_err() {
                reply.error(fuser::Errno::EIO);
                return;
            }
            match files
                .with_tree(generation, |tree| {
                    tree.node(ino.0).map(|node| node_attr(tree, ino.0, node))
                })
                .flatten()
            {
                Some(attr) => reply.attr(&Duration::ZERO, &attr),
                None => reply.error(fuser::Errno::ENOENT),
            }
        });
    }

    fn lookup(
        &self,
        _req: &fuser::Request,
        parent: fuser::INodeNo,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEntry,
    ) {
        // Every name in the tree arrived as UTF-16 on the wire, so a name that
        // is not valid UTF-8 cannot be in it.
        let found = name.to_str().and_then(|name| {
            self.files
                .with_tree(self.generation, |tree| tree.lookup(parent.0, name))
                .flatten()
        });
        let Some(inode) = found else {
            reply.error(fuser::Errno::ENOENT);
            return;
        };
        let files = self.files.clone();
        let generation = self.generation;
        self.files.runtime.spawn(async move {
            if files.size(generation, inode).await.is_err() {
                reply.error(fuser::Errno::EIO);
                return;
            }
            match files
                .with_tree(generation, |tree| {
                    tree.node(inode).map(|node| node_attr(tree, inode, node))
                })
                .flatten()
            {
                Some(attr) => reply.entry(&Duration::ZERO, &attr, fuser::Generation(generation)),
                None => reply.error(fuser::Errno::ENOENT),
            }
        });
    }

    fn readdir(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let listing = self
            .files
            .with_tree(self.generation, |tree| {
                let Some(node) = tree.node(ino.0) else {
                    return Err(fuser::Errno::ENOENT);
                };
                if !node.is_directory() {
                    return Err(fuser::Errno::ENOTDIR);
                }
                let mut entries = vec![
                    (ino.0, fuser::FileType::Directory, ".".to_owned()),
                    (node.parent, fuser::FileType::Directory, "..".to_owned()),
                ];
                entries.extend(tree.children(ino.0).iter().filter_map(|inode| {
                    let child = tree.node(*inode)?;
                    Some((*inode, file_type(child), child.name.clone()))
                }));
                Ok(entries)
            })
            .unwrap_or(Err(fuser::Errno::ENOENT));
        let entries = match listing {
            Ok(entries) => entries,
            Err(errno) => {
                reply.error(errno);
                return;
            }
        };
        for (entry_offset, (inode, kind, name)) in
            entries.into_iter().enumerate().skip(offset as usize)
        {
            if reply.add(fuser::INodeNo(inode), (entry_offset + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    ) {
        match self
            .files
            .with_tree(self.generation, |tree| {
                tree.node(ino.0).map(|node| node.kind)
            })
            .flatten()
        {
            // Direct I/O so read sizes reach us unmodified and nothing is
            // cached on top of content fetched one range at a time.
            Some(RemoteNodeKind::File { .. }) => {
                reply.opened(fuser::FileHandle(0), fuser::FopenFlags::FOPEN_DIRECT_IO)
            }
            Some(RemoteNodeKind::Directory) => reply.error(fuser::Errno::EISDIR),
            None => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyData,
    ) {
        let files = self.files.clone();
        let generation = self.generation;
        self.files.runtime.spawn(async move {
            match files.read(generation, ino.0, offset, size).await {
                Ok(data) => reply.data(&data),
                _ => reply.error(fuser::Errno::EIO),
            }
        });
    }
}

fn file_type(node: &RemoteNode) -> fuser::FileType {
    if node.is_directory() {
        fuser::FileType::Directory
    } else {
        fuser::FileType::RegularFile
    }
}

fn node_attr(tree: &RemoteTree, inode: u64, node: &RemoteNode) -> fuser::FileAttr {
    fuser::FileAttr {
        ino: fuser::INodeNo(inode),
        size: node.size.unwrap_or(0),
        blocks: node.size.unwrap_or(0).div_ceil(512),
        atime: node.modified,
        mtime: node.modified,
        ctime: node.modified,
        crtime: node.modified,
        kind: file_type(node),
        perm: node.permissions(),
        nlink: tree.link_count(inode),
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}
