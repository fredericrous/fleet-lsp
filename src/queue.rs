//! Byte-bounded frame queues between the relay's threads.
//!
//! A frame's bytes stay charged to its queue from push until the writer
//! reports the write finished (`release`), so a writer blocked mid-write on a
//! frame does not let the queue fill a second time behind it: the memory
//! bound counts in-flight frames too.
//!
//! A queue has one or two lanes, each with its own byte cap. The client-output
//! queue uses lane `OWN` for replies fleet-lsp writes itself (a reserved
//! share server traffic cannot use) and lane `SERVER` for relayed frames;
//! `pop` serves `OWN` first.
//!
//! A frame larger than a lane's cap (up to `frame::MAX_FRAME`) is admitted
//! only into an empty lane, so one large frame can always pass and the bound
//! stays `cap.max(MAX_FRAME)` per lane.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) const OWN: usize = 0;
pub(crate) const SERVER: usize = 1;

#[derive(Debug)]
struct Lane {
    items: VecDeque<Vec<u8>>,
    charged: usize,
    cap: usize,
}

#[derive(Debug)]
struct Inner {
    lanes: Vec<Lane>,
    closed: bool,
    /// Since when the queue has held bytes without a write completing.
    stalled_since: Option<Instant>,
}

#[derive(Debug)]
pub(crate) struct Queue {
    inner: Mutex<Inner>,
    cv: Condvar,
}

/// A popped frame: its lane and bytes. Pass it back to `release` once the
/// write has finished.
#[derive(Debug)]
pub(crate) struct Popped {
    pub(crate) lane: usize,
    pub(crate) body: Vec<u8>,
}

impl Queue {
    pub(crate) fn single(cap: usize) -> Queue {
        Queue::with_lanes(&[cap])
    }

    pub(crate) fn two_lane(own_cap: usize, server_cap: usize) -> Queue {
        Queue::with_lanes(&[own_cap, server_cap])
    }

    fn with_lanes(caps: &[usize]) -> Queue {
        Queue {
            inner: Mutex::new(Inner {
                lanes: caps
                    .iter()
                    .map(|&cap| Lane {
                        items: VecDeque::new(),
                        charged: 0,
                        cap,
                    })
                    .collect(),
                closed: false,
                stalled_since: None,
            }),
            cv: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn admits(lane: &Lane, len: usize) -> bool {
        lane.charged + len <= lane.cap || (lane.charged == 0 && len <= crate::frame::MAX_FRAME)
    }

    /// Never blocks: `Err` hands the frame back when the lane is full.
    pub(crate) fn try_push(&self, lane: usize, body: Vec<u8>) -> Result<(), Vec<u8>> {
        let mut g = self.lock();
        if g.closed || !Self::admits(&g.lanes[lane], body.len()) {
            return Err(body);
        }
        Self::enqueue(&mut g, lane, body);
        drop(g);
        self.cv.notify_all();
        Ok(())
    }

    /// Blocks until the lane has room. `Err` if the queue was closed.
    pub(crate) fn push(&self, lane: usize, body: Vec<u8>) -> Result<(), Vec<u8>> {
        let mut g = self.lock();
        loop {
            if g.closed {
                return Err(body);
            }
            if Self::admits(&g.lanes[lane], body.len()) {
                Self::enqueue(&mut g, lane, body);
                drop(g);
                self.cv.notify_all();
                return Ok(());
            }
            g = self.cv.wait(g).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn enqueue(g: &mut Inner, lane: usize, body: Vec<u8>) {
        if g.stalled_since.is_none() {
            g.stalled_since = Some(Instant::now());
        }
        let l = &mut g.lanes[lane];
        l.charged += body.len();
        l.items.push_back(body);
    }

    /// Blocks for the next frame, lane `OWN` first. `None` once the queue is
    /// closed and empty.
    pub(crate) fn pop(&self) -> Option<Popped> {
        let mut g = self.lock();
        loop {
            for (lane, l) in g.lanes.iter_mut().enumerate() {
                if let Some(body) = l.items.pop_front() {
                    return Some(Popped { lane, body });
                }
            }
            if g.closed {
                return None;
            }
            g = self.cv.wait(g).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Non-blocking pop, for the event loop.
    pub(crate) fn try_pop(&self) -> Option<Popped> {
        let mut g = self.lock();
        for (lane, l) in g.lanes.iter_mut().enumerate() {
            if let Some(body) = l.items.pop_front() {
                return Some(Popped { lane, body });
            }
        }
        None
    }

    /// The write of `len` bytes from `lane` finished: uncharge them.
    pub(crate) fn release(&self, lane: usize, len: usize) {
        let mut g = self.lock();
        let l = &mut g.lanes[lane];
        l.charged = l.charged.saturating_sub(len);
        let empty = g.lanes.iter().all(|l| l.charged == 0);
        g.stalled_since = if empty { None } else { Some(Instant::now()) };
        drop(g);
        self.cv.notify_all();
    }

    /// Stops accepting; `pop` drains what is left, then returns `None`.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_all();
    }

    /// How long the queue has held bytes without a write completing.
    pub(crate) fn stalled_for(&self, now: Instant) -> Option<Duration> {
        self.lock()
            .stalled_since
            .map(|t| now.saturating_duration_since(t))
    }

    pub(crate) fn is_drained(&self) -> bool {
        self.lock().lanes.iter().all(|l| l.charged == 0)
    }

    #[cfg(test)]
    fn charged(&self, lane: usize) -> usize {
        self.lock().lanes[lane].charged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn bytes_stay_charged_until_released() {
        let q = Queue::single(10);
        q.try_push(0, vec![0; 6]).unwrap();
        let p = q.try_pop().unwrap();
        // Popped but not yet written: still charged, so 6 more do not fit.
        assert!(q.try_push(0, vec![0; 6]).is_err());
        q.release(p.lane, p.body.len());
        assert_eq!(q.charged(0), 0);
        assert!(q.try_push(0, vec![0; 6]).is_ok());
    }

    #[test]
    fn oversize_frame_only_into_an_empty_lane() {
        let q = Queue::single(10);
        q.try_push(0, vec![0; 1]).unwrap();
        assert!(q.try_push(0, vec![0; 20]).is_err());
        let p = q.try_pop().unwrap();
        q.release(p.lane, 1);
        assert!(q.try_push(0, vec![0; 20]).is_ok());
    }

    #[test]
    fn own_lane_is_reserved_and_served_first() {
        let q = Queue::two_lane(4, 8);
        q.try_push(SERVER, vec![1; 8]).unwrap();
        assert!(q.try_push(SERVER, vec![1; 1]).is_err(), "server lane full");
        q.try_push(OWN, vec![2; 4]).unwrap();
        assert_eq!(q.try_pop().unwrap().lane, OWN);
    }

    #[test]
    fn blocking_push_waits_for_release() {
        let q = Arc::new(Queue::single(4));
        q.try_push(0, vec![0; 4]).unwrap();
        let q2 = Arc::clone(&q);
        let t = thread::spawn(move || q2.push(0, vec![0; 4]).is_ok());
        thread::sleep(Duration::from_millis(50));
        let p = q.pop().unwrap();
        q.release(p.lane, p.body.len());
        assert!(t.join().unwrap());
    }

    #[test]
    fn close_drains_then_ends() {
        let q = Queue::single(10);
        q.try_push(0, vec![0; 1]).unwrap();
        q.close();
        assert!(q.pop().is_some());
        assert!(q.pop().is_none());
        assert!(q.try_push(0, vec![0]).is_err());
    }

    #[test]
    fn stall_clock_runs_only_while_bytes_wait() {
        let q = Queue::single(10);
        let now = Instant::now();
        assert_eq!(q.stalled_for(now), None);
        q.try_push(0, vec![0; 1]).unwrap();
        assert!(q.stalled_for(Instant::now()).is_some());
        let p = q.try_pop().unwrap();
        q.release(p.lane, 1);
        assert_eq!(q.stalled_for(Instant::now()), None);
    }
}
