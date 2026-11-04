//! Bounded audio queue shared by the decoder and ALSA output.
use super::audio::AudioChunk;
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct State {
    chunks: VecDeque<AudioChunk>,
    sender_alive: bool,
    receiver_alive: bool,
}
struct Queue {
    state: Mutex<State>,
    ready: Condvar,
    space: Condvar,
    capacity: usize,
    live: bool,
    count: Arc<AtomicUsize>,
}
pub(super) struct Sender(Arc<Queue>);
pub(super) struct Receiver(Arc<Queue>);
#[derive(Default)]
pub(super) struct Discarded {
    pub chunks: u64,
    pub samples: u64,
}

pub(super) fn channel(capacity: usize, live: bool, count: Arc<AtomicUsize>) -> (Sender, Receiver) {
    let queue = Arc::new(Queue {
        state: Mutex::new(State {
            chunks: VecDeque::with_capacity(capacity),
            sender_alive: true,
            receiver_alive: true,
        }),
        ready: Condvar::new(),
        space: Condvar::new(),
        capacity,
        live,
        count,
    });
    (Sender(queue.clone()), Receiver(queue))
}

impl Sender {
    pub fn send(&self, chunk: AudioChunk) -> io::Result<Discarded> {
        let queue = &self.0;
        let mut state = queue.state.lock().unwrap();
        while !queue.live && state.receiver_alive && state.chunks.len() >= queue.capacity {
            state = queue.space.wait(state).unwrap();
        }
        if !state.receiver_alive {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "audio output stopped",
            ));
        }
        let mut discarded = Discarded::default();
        if state.chunks.len() >= queue.capacity {
            let old = state.chunks.pop_front().unwrap();
            discarded.chunks = 1;
            discarded.samples = (old.samples.len() / 2) as u64;
        }
        state.chunks.push_back(chunk);
        queue.count.store(state.chunks.len(), Ordering::Relaxed);
        queue.ready.notify_one();
        Ok(discarded)
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().sender_alive = false;
        self.0.ready.notify_all();
    }
}
impl Receiver {
    pub fn recv(&self) -> Option<AudioChunk> {
        let queue = &self.0;
        let mut state = queue.state.lock().unwrap();
        while state.chunks.is_empty() && state.sender_alive {
            state = queue.ready.wait(state).unwrap();
        }
        let chunk = state.chunks.pop_front();
        queue.count.store(state.chunks.len(), Ordering::Relaxed);
        queue.space.notify_one();
        chunk
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.receiver_alive = false;
        state.chunks.clear();
        self.0.count.store(0, Ordering::Relaxed);
        self.0.space.notify_all();
    }
}
