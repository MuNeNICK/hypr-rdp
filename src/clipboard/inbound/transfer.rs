//! Application-owned read admission and correlation, using IronRDP's fetch/PDU APIs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::chunked_fetch::{ChunkedFetch, ChunkedFetchProgress};
use ironrdp_cliprdr::pdu::{
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
};
use ironrdp_server::ServerEvent;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use super::tree::{RemoteNodeKind, RemoteTree};

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PENDING: usize = 64;
const MAX_BUFFERED: usize = 16 * 1024 * 1024;
const MAX_READ: u32 = 8 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct Transfer {
    state: Arc<Mutex<State>>,
    sender: mpsc::UnboundedSender<ServerEvent>,
    pub(super) runtime: Handle,
    max_entries: usize,
    max_chunk: u32,
    timeout: Duration,
}

struct State {
    generation: u64,
    closed: bool,
    stream: bool,
    huge: bool,
    tree: RemoteTree,
    data_id: Option<u32>,
    next_stream: Option<u32>,
    pending: HashMap<u32, Pending>,
}

enum Operation {
    Size { inode: u64 },
    Range { base: u64, fetch: ChunkedFetch },
}

enum Value {
    Size(u64),
    Bytes(Vec<u8>),
}
struct Pending {
    operation: Operation,
    answer: oneshot::Sender<Result<Value, ()>>,
    reserved: usize,
    _cancel_timeout: oneshot::Sender<()>,
}

impl State {
    fn current(&self, generation: u64) -> bool {
        !self.closed && self.stream && self.generation == generation
    }

    fn invalidate(&mut self) {
        self.pending.clear(); // Closing answers wakes every waiting filesystem request.
        self.tree = RemoteTree::build(&[], 0, self.tree.next_base());
        match self.generation.checked_add(1) {
            Some(next) => self.generation = next,
            None => self.closed = true,
        }
        self.data_id = None;
    }
}

impl Transfer {
    pub(super) fn new(
        sender: mpsc::UnboundedSender<ServerEvent>,
        max_entries: usize,
        max_chunk: u32,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                generation: 0,
                closed: false,
                stream: false,
                huge: false,
                tree: RemoteTree::empty(),
                data_id: None,
                next_stream: Some(1),
                pending: HashMap::new(),
            })),
            sender,
            runtime: Handle::current(),
            max_entries,
            max_chunk: max_chunk.max(1),
            timeout: READ_TIMEOUT,
        }
    }

    pub(super) fn capabilities(&self, stream: bool, huge: bool) {
        if let Ok(mut state) = self.state.lock() {
            if state.stream != stream || state.huge != huge {
                state.invalidate();
            }
            state.stream = stream;
            state.huge = huge;
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.stream && !state.closed)
    }

    pub(super) fn invalidate(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.invalidate();
        }
    }

    pub(super) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.invalidate();
            state.closed = true;
        }
    }

    pub(super) fn accept(
        &self,
        files: &[FileDescriptor],
        data_id: Option<u32>,
    ) -> Option<(u64, Vec<String>)> {
        let mut state = self.state.lock().ok()?;
        state.invalidate();
        if !state.current(state.generation) {
            return None;
        }
        state.tree = RemoteTree::build(files, self.max_entries, state.tree.next_base());
        state.data_id = data_id;
        let roots = state
            .tree
            .roots()
            .iter()
            .filter_map(|inode| state.tree.node(*inode).map(|n| n.name.clone()))
            .collect();
        Some((state.generation, roots))
    }

    /// The closure and invalidation are serialized, including publication to Wayland.
    pub(super) fn with_tree<T>(
        &self,
        generation: u64,
        f: impl FnOnce(&RemoteTree) -> T,
    ) -> Option<T> {
        let state = self.state.lock().ok()?;
        state.current(generation).then(|| f(&state.tree))
    }

    pub(super) async fn size(&self, generation: u64, inode: u64) -> Result<u64, ()> {
        let receiver = {
            let mut state = self.state.lock().map_err(|_| ())?;
            if !state.current(generation) {
                return Err(());
            }
            let node = state.tree.node(inode).ok_or(())?;
            if let Some(size) = node.size {
                return if !state.huge && size > u64::from(u32::MAX) {
                    Err(())
                } else {
                    Ok(size)
                };
            }
            let RemoteNodeKind::File { index } = node.kind else {
                return Err(());
            };
            let id = Self::allocate(&mut state, 8)?;
            let request = FileContentsRequest {
                stream_id: id,
                index,
                flags: FileContentsFlags::SIZE,
                position: 0,
                requested_size: 8,
                data_id: state.data_id,
            };
            self.submit(&mut state, request, Operation::Size { inode }, 8)?
        };
        match receiver.await.map_err(|_| ())?? {
            Value::Size(size) => Ok(size),
            _ => Err(()),
        }
    }

    pub(super) async fn read(
        &self,
        generation: u64,
        inode: u64,
        position: u64,
        size: u32,
    ) -> Result<Vec<u8>, ()> {
        self.size(generation, inode).await?;
        let receiver = {
            let mut state = self.state.lock().map_err(|_| ())?;
            if !state.current(generation) || size > MAX_READ {
                return Err(());
            }
            let node = state.tree.node(inode).ok_or(())?;
            let RemoteNodeKind::File { index } = node.kind else {
                return Err(());
            };
            let amount = node
                .size
                .ok_or(())?
                .saturating_sub(position)
                .min(u64::from(size));
            if amount == 0 {
                return Ok(Vec::new());
            }
            let id = Self::allocate(&mut state, amount as usize)?;
            let mut fetch = ChunkedFetch::new(
                id,
                index,
                amount,
                self.max_chunk,
                state.data_id,
                u64::from(MAX_READ),
            );
            let mut request = fetch.next_request().ok_or(())?;
            request.position = position;
            self.submit(
                &mut state,
                request,
                Operation::Range {
                    base: position,
                    fetch,
                },
                amount as usize,
            )?
        };
        match receiver.await.map_err(|_| ())?? {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(()),
        }
    }

    fn allocate(state: &mut State, bytes: usize) -> Result<u32, ()> {
        // Prune consumers that have gone away before applying either budget.
        state
            .pending
            .retain(|_, pending| !pending.answer.is_closed());
        if state.pending.len() >= MAX_PENDING
            || bytes + state.pending.values().map(|p| p.reserved).sum::<usize>() > MAX_BUFFERED
        {
            return Err(());
        }
        let id = state.next_stream.ok_or(())?;
        state.next_stream = id.checked_add(1); // A late response can never alias a new request.
        Ok(id)
    }

    fn submit(
        &self,
        state: &mut State,
        request: FileContentsRequest,
        operation: Operation,
        reserved: usize,
    ) -> Result<oneshot::Receiver<Result<Value, ()>>, ()> {
        let id = request.stream_id;
        let (answer, receiver) = oneshot::channel();
        let (cancel, cancelled) = oneshot::channel();
        self.sender
            .send(ServerEvent::Clipboard(
                ClipboardMessage::SendFileContentsRequest(request),
            ))
            .map_err(|_| ())?;
        state.pending.insert(
            id,
            Pending {
                operation,
                answer,
                reserved,
                _cancel_timeout: cancel,
            },
        );
        let shared = Arc::clone(&self.state);
        let timeout = self.timeout;
        self.runtime.spawn(async move {
            tokio::select! {
                _ = cancelled => {},
                _ = tokio::time::sleep(timeout) => {
                    if let Ok(mut state) = shared.lock() { state.pending.remove(&id); }
                }
            }
        });
        Ok(receiver)
    }

    pub(super) fn on_response(&self, response: FileContentsResponse<'_>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(mut pending) = state.pending.remove(&response.stream_id()) else {
            return;
        };
        if pending.answer.is_closed() {
            return;
        }
        let result = match &mut pending.operation {
            Operation::Size { inode } => {
                if response.is_error() {
                    Err(())
                } else if let Ok(bytes) = <[u8; 8]>::try_from(response.data()) {
                    let size = u64::from_le_bytes(bytes);
                    if (!state.huge && size > u64::from(u32::MAX))
                        || !state.tree.set_size(*inode, size)
                    {
                        Err(())
                    } else {
                        Ok(Value::Size(size))
                    }
                } else {
                    Err(())
                }
            }
            Operation::Range { base, fetch } => match fetch.on_response(&response) {
                ChunkedFetchProgress::Failed => Err(()),
                ChunkedFetchProgress::Complete => {
                    let Operation::Range { fetch, .. } = pending.operation else {
                        unreachable!()
                    };
                    let _ = pending.answer.send(Ok(Value::Bytes(fetch.into_data())));
                    return;
                }
                ChunkedFetchProgress::InProgress => {
                    if let Some(mut request) = fetch.next_request() {
                        if let Some(position) = base.checked_add(request.position) {
                            request.position = position;
                            if self
                                .sender
                                .send(ServerEvent::Clipboard(
                                    ClipboardMessage::SendFileContentsRequest(request),
                                ))
                                .is_ok()
                            {
                                state.pending.insert(response.stream_id(), pending);
                                return;
                            }
                        }
                    }
                    Err(())
                }
            },
        };
        let _ = pending.answer.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn generated_inbound_replacement_sequences_do_not_reuse_requests(
            replacements in proptest::collection::vec(any::<bool>(), 0..24),
        ) {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let (transfer, mut events, mut generation, mut inode) = setup(&[FileDescriptor::new("same-name").with_file_size(1)], 1);
                let mut last_stream = 0;
                for replace in replacements {
                    let pending = read(&transfer, generation, inode, 0, 1);
                    let req = request(&mut events).await;
                    assert!(req.stream_id > last_stream);
                    last_stream = req.stream_id;
                    if replace {
                        transfer.invalidate();
                        let (next, _) = transfer.accept(&[FileDescriptor::new("same-name").with_file_size(1)], None).unwrap();
                        assert!(transfer.read(generation, inode, 0, 1).await.is_err());
                        generation = next;
                        inode = transfer.with_tree(generation, |tree| tree.roots()[0]).unwrap();
                    }
                    transfer.on_response(FileContentsResponse::new_data_response(req.stream_id, b"x".to_vec()));
                    let result = pending.await.unwrap();
                    assert_eq!(result.is_err(), replace);
                    if !replace { assert_eq!(result.unwrap(), b"x"); }
                    assert!(events.try_recv().is_err());
                }
                transfer.close();
                transfer.capabilities(true, true);
                assert!(!transfer.enabled());
                assert!(transfer.read(generation, inode, 0, 1).await.is_err());
            });
        }
    }

    fn setup(
        files: &[FileDescriptor],
        chunk: u32,
    ) -> (Transfer, mpsc::UnboundedReceiver<ServerEvent>, u64, u64) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let transfer = Transfer::new(sender, 100, chunk);
        transfer.capabilities(true, true);
        let (generation, _) = transfer.accept(files, Some(77)).unwrap();
        let inode = transfer
            .with_tree(generation, |tree| tree.roots()[0])
            .unwrap();
        (transfer, receiver, generation, inode)
    }

    async fn request(receiver: &mut mpsc::UnboundedReceiver<ServerEvent>) -> FileContentsRequest {
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendFileContentsRequest(request))) =
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
        else {
            panic!("expected file contents request")
        };
        request
    }

    fn read(
        transfer: &Transfer,
        generation: u64,
        inode: u64,
        position: u64,
        size: u32,
    ) -> tokio::task::JoinHandle<Result<Vec<u8>, ()>> {
        let transfer = transfer.clone();
        tokio::spawn(async move { transfer.read(generation, inode, position, size).await })
    }

    #[tokio::test]
    async fn inbound_known_and_unknown_size_reads() {
        for size in [Some(5), None] {
            let mut file = FileDescriptor::new("file");
            file.file_size = size;
            let (transfer, mut events, generation, inode) = setup(&[file], 100);
            let read = read(&transfer, generation, inode, 1, 100);
            if size.is_none() {
                let req = request(&mut events).await;
                assert_eq!(req.flags, FileContentsFlags::SIZE);
                assert_eq!(
                    (req.position, req.requested_size, req.data_id),
                    (0, 8, Some(77))
                );
                transfer.on_response(FileContentsResponse::new_size_response(req.stream_id, 5));
            }
            let req = request(&mut events).await;
            assert_eq!(req.flags, FileContentsFlags::RANGE);
            assert_eq!(
                (req.index, req.position, req.requested_size, req.data_id),
                (0, 1, 4, Some(77))
            );
            transfer.on_response(FileContentsResponse::new_data_response(
                req.stream_id,
                b"BCDE".to_vec(),
            ));
            assert_eq!(read.await.unwrap().unwrap(), b"BCDE");
            assert!(transfer
                .read(generation, inode, 5, 10)
                .await
                .unwrap()
                .is_empty());
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn inbound_short_range_progress() {
        let (transfer, mut events, generation, inode) =
            setup(&[FileDescriptor::new("file").with_file_size(100)], 3);
        let read = read(&transfer, generation, inode, 20, 7);
        for (offset, requested, data) in
            [(20, 3, b"ab".as_slice()), (22, 3, b"cde"), (25, 2, b"fg")]
        {
            let req = request(&mut events).await;
            assert_eq!((req.position, req.requested_size), (offset, requested));
            transfer.on_response(FileContentsResponse::new_data_response(
                req.stream_id,
                data.to_vec(),
            ));
        }
        assert_eq!(read.await.unwrap().unwrap(), b"abcdefg");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn inbound_rejects_stale_inode_and_closed_reads() {
        let (transfer, mut events, generation, inode) =
            setup(&[FileDescriptor::new("old").with_file_size(5)], 5);
        let pending = read(&transfer, generation, inode, 0, 5);
        let old = request(&mut events).await;
        transfer.invalidate();
        assert!(pending.await.unwrap().is_err());
        let (new_generation, _) = transfer
            .accept(&[FileDescriptor::new("new").with_file_size(5)], None)
            .unwrap();
        assert!(transfer.read(generation, inode, 0, 5).await.is_err());
        assert!(transfer.read(new_generation, inode, 0, 5).await.is_err());
        transfer.on_response(FileContentsResponse::new_data_response(
            old.stream_id,
            b"wrong".to_vec(),
        ));
        assert!(events.try_recv().is_err());
        transfer.close();
        assert!(transfer
            .accept(&[FileDescriptor::new("closed")], None)
            .is_none());
        assert!(transfer.read(new_generation, inode, 0, 5).await.is_err());
    }

    #[tokio::test]
    async fn inbound_timeout_and_response_bounds() {
        let (mut transfer, mut events, generation, inode) =
            setup(&[FileDescriptor::new("file").with_file_size(10)], 4);
        for data in [b"12345".as_slice(), b""] {
            let pending = read(&transfer, generation, inode, 0, 4);
            let req = request(&mut events).await;
            transfer.on_response(FileContentsResponse::new_data_response(
                req.stream_id + 1,
                b"late".to_vec(),
            ));
            assert!(!pending.is_finished());
            transfer.on_response(FileContentsResponse::new_data_response(
                req.stream_id,
                data.to_vec(),
            ));
            assert!(pending.await.unwrap().is_err());
        }
        transfer.timeout = Duration::from_millis(10);
        let pending = read(&transfer, generation, inode, 0, 4);
        let req = request(&mut events).await;
        assert!(tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        transfer.on_response(FileContentsResponse::new_data_response(
            req.stream_id,
            b"late".to_vec(),
        ));
        assert!(transfer.state.lock().unwrap().pending.is_empty());
        drop(events);
        assert!(transfer.read(generation, inode, 0, 4).await.is_err());
    }

    #[tokio::test]
    async fn inbound_pending_budget() {
        let (transfer, mut events, generation, inode) = setup(
            &[FileDescriptor::new("file").with_file_size(u64::from(MAX_READ))],
            4,
        );
        let mut held = Vec::new();
        for _ in 0..MAX_PENDING {
            held.push(read(&transfer, generation, inode, 0, 4));
            request(&mut events).await;
        }
        assert!(transfer.read(generation, inode, 0, 4).await.is_err());
        assert!(events.try_recv().is_err());
        transfer.invalidate();
        for pending in held {
            assert!(pending.await.unwrap().is_err());
        }

        let (generation, _) = transfer
            .accept(
                &[FileDescriptor::new("large").with_file_size(u64::from(MAX_READ))],
                None,
            )
            .unwrap();
        let inode = transfer
            .with_tree(generation, |tree| tree.roots()[0])
            .unwrap();
        let one = read(&transfer, generation, inode, 0, MAX_READ);
        request(&mut events).await;
        let two = read(&transfer, generation, inode, 0, MAX_READ);
        request(&mut events).await;
        assert!(transfer.read(generation, inode, 0, 1).await.is_err());
        one.abort();
        assert!(one.await.unwrap_err().is_cancelled());
        let recovered = read(&transfer, generation, inode, 0, 1);
        let req = request(&mut events).await;
        transfer.on_response(FileContentsResponse::new_data_response(
            req.stream_id,
            b"x".to_vec(),
        ));
        assert_eq!(recovered.await.unwrap().unwrap(), b"x");
        transfer.close();
        assert!(two.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn inbound_size_failure_and_stream_exhaustion_are_terminal_for_the_request() {
        let (transfer, mut events, generation, inode) = setup(&[FileDescriptor::new("unknown")], 4);
        let pending = read(&transfer, generation, inode, 0, 1);
        let req = request(&mut events).await;
        transfer.on_response(FileContentsResponse::new_data_response(
            req.stream_id,
            vec![0; 7],
        ));
        assert!(pending.await.unwrap().is_err());
        transfer.state.lock().unwrap().next_stream = Some(u32::MAX);
        let pending = read(&transfer, generation, inode, 0, 1);
        let req = request(&mut events).await;
        assert_eq!(req.stream_id, u32::MAX);
        transfer.on_response(FileContentsResponse::new_error(req.stream_id));
        assert!(pending.await.unwrap().is_err());
        assert!(transfer.read(generation, inode, 0, 1).await.is_err());
        assert!(events.try_recv().is_err());
    }
}
