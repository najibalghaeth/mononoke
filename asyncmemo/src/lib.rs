// Copyright (c) 2017-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! Asynchronous Memoizing Cache
//!
//! This crate implements a cache to memoize results calculated by some async process.
//!
//! The primary access method is `Asyncmemo::get()`. If the result has been previously calculated
//! and cached, then the result is directly returned from the cache.
//!
//! Otherwise an implementation of `Filler` which produces new results. Since the process
//! constructing the result is async, each query to fetch the result will poll the process
//! and will update the cache when it finishes.
//!
//! If a value is requested from the cache while the value is being computed, then that task
//! will be added to a notification list and will be woken when the computation changes state
//! (either completes, fails, or makes progress in its async state machine).
//!
//! The `fill()` function returns an instance of `Future`, and as such can fail. In that case
//! no result will be cached, and the error will be returned to the task that's currently
//! calling that future's `poll()`. Other tasks waiting will be woken, but they'll simply get
//! cache miss and will attempt to compute the result again. There is no negative caching
//! or rate limiting, so if process is prone to failure then it can "succeed" but return a
//! sentinel value representing the failure which the application can handle with its own logic.
//!
//! TODO: add interface to allow multiple implementations of the underlying cache, to allow
//!   eviction and other policies to be controlled.
//!
//! TODO: entry invalidation interface
#![deny(warnings)]

extern crate futures;
extern crate futures_ext;
extern crate heapsize;
extern crate linked_hash_map;
#[cfg(test)]
extern crate tokio_timer;

use std::fmt::{self, Debug};
use std::hash::Hash;
use std::mem;
use std::sync::{Arc, Mutex};
use std::thread;
use std::usize;

use futures::{Async, Future, Poll};
use futures::future::{IntoFuture, Shared, SharedError, SharedItem};
use futures::task::{self, Task};

#[cfg(test)]
mod test;

mod boundedhash;
mod weight;

use boundedhash::BoundedHash;
pub use weight::Weight;

/// Asynchronous memoizing cache for async processes
///
/// The cache requires an instance of an implementation of the `Filler` trait
/// to generate new results.
pub struct Asyncmemo<F>
where
    F: Filler,
    F::Key: Eq + Hash,
{
    inner: Arc<AsyncmemoInner<F>>,
}

impl<F> Debug for Asyncmemo<F>
where
    F: Filler,
    F::Key: Eq + Hash + Debug,
    <<F as Filler>::Value as IntoFuture>::Future: Debug,
    <<F as Filler>::Value as IntoFuture>::Item: Debug,
    <<F as Filler>::Value as IntoFuture>::Error: Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Asyncmemo")
            .field("inner", &self.inner)
            .finish()
    }
}

/// Generate a result for the cache.
///
/// The function implemented by `fill()` should be referentially transparent - the output
/// should only depend on the value of the `key` parameter.
/// It may fail - the failure will be propagated to one of the callers, and the result won't
/// be cached.
pub trait Filler: Sized {
    type Key: Eq + Hash;
    type Value: IntoFuture + 'static;

    fn fill(&self, cache: &Asyncmemo<Self>, key: &Self::Key) -> Self::Value;
}

type FillerSlot<F> = Slot<
    <<<F as Filler>::Value as IntoFuture>::Future as Future>::Item,
    <<<F as Filler>::Value as IntoFuture>::Future as Future>::Error,
>;

// We really want a type bound on F, but currently that emits an annoying E0122 warning
type CacheHash<F> = BoundedHash<<F as Filler>::Key, FillerSlot<F>>;

struct AsyncmemoInner<F>
where
    F: Filler,
    F::Key: Eq + Hash,
{
    hash: Mutex<CacheHash<F>>,
    filler: F,
}

impl<F> Debug for AsyncmemoInner<F>
where
    F: Filler,
    F::Key: Eq + Hash + Debug,
    <<F as Filler>::Value as IntoFuture>::Future: Debug,
    <<F as Filler>::Value as IntoFuture>::Error: Debug,
    <<F as Filler>::Value as IntoFuture>::Item: Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let hash = self.hash.lock().expect("poisoned lock");
        fmt.debug_struct("AsyncmemoInner")
            .field("hash", &*hash)
            .finish()
    }
}

// User-supplied future is wrapped into SharedAsyncmemoFuture. With that the internal future can
// be polled by many MemoFutures at once. Note that error type of the Shared future is SharedError.
// This type derefs to the underlying error, but not all underlying errors implement clone
// (for example, failure Error is not cloneable).
// That means that we have a few options: return SharedError to a user (undesirable) or use some
// hacks to overcome this restriction. We've chosen the second option - see SharedAsyncmemoError
// below.

struct SharedAsyncmemoFuture<Item, Error> {
    // Future can only be None when it's dropped (see Drop implementation)
    future: Option<Shared<Box<Future<Item = Item, Error = SharedAsyncmemoError<Error>>>>>,
}

impl<Item, Error> SharedAsyncmemoFuture<Item, Error> {
    fn new(future: Shared<Box<Future<Item = Item, Error = SharedAsyncmemoError<Error>>>>) -> Self {
        SharedAsyncmemoFuture {
            future: Some(future),
        }
    }
}

impl<Item, Error> Future for SharedAsyncmemoFuture<Item, Error> {
    type Item = SharedItem<Item>;
    type Error = SharedError<SharedAsyncmemoError<Error>>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.future {
            Some(ref mut future) => future.poll(),
            None => panic!("unexpected state"),
        }
    }
}

impl<Item, Error> Clone for SharedAsyncmemoFuture<Item, Error> {
    fn clone(&self) -> Self {
        match self.future {
            Some(ref future) => SharedAsyncmemoFuture::new(future.clone()),
            None => panic!("unexpected state"),
        }
    }
}

impl<Item, Error> Drop for SharedAsyncmemoFuture<Item, Error> {
    fn drop(&mut self) {
        if thread::panicking() {
            // Shared future grabs a lock during the Drop. The lock is poisoned when thread
            // panics, and that aborts the process. In turn that causes #[should_panic] tests
            // to fail. Workaround the problem by forgetting shared future if the thread is
            // panicking.
            let future = self.future.take().unwrap();
            std::mem::forget(future);
        }
    }
}

// Asyncmemo doesn't do negative caching. So the first MemoFuture that polls the underlying errored
// SharedFuture grabs the lock and replaces Some(err) with None. This first future then returns
// error to the user, but others user Filler to start new Future instead.
type SharedAsyncmemoError<Error> = Mutex<Option<Error>>;

// Result of polling SharedAsyncmemoFuture: either it returns normal poll result
// (i.e. Ready, NotReady, Err), or the fact that the error was already processed by another future
enum SharedAsyncmemoFuturePoll<Item, Error> {
    PollResult(Poll<Item, Error>),
    MovedError,
}

fn wrap_filler_future<Fut: Future + 'static>(
    fut: Fut,
) -> SharedAsyncmemoFuture<<Fut as Future>::Item, <Fut as Future>::Error> {
    let fut: Box<
        Future<Item = <Fut as Future>::Item, Error = SharedAsyncmemoError<<Fut as Future>::Error>>,
    > = Box::new(fut.map_err(|err| Mutex::new(Some(err))));
    SharedAsyncmemoFuture::new(fut.shared())
}

enum Slot<Item, Error> {
    Waiting(SharedAsyncmemoFuture<Item, Error>), // waiting for entry to become available
    Polling(Vec<Task>), // Future currently being polled, with waiting Tasks
    Complete(Item),     // got value
}

impl<Item, Error> Debug for Slot<Item, Error> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Slot::Waiting(..) => fmt.write_str("Waiting"),
            &Slot::Polling(..) => fmt.write_str("Polling"),
            &Slot::Complete(..) => fmt.write_str("Complete"),
        }
    }
}

impl<Item, Error> Slot<Item, Error> {
    fn is_waiting(&self) -> bool {
        match self {
            &Slot::Waiting(..) => true,
            _ => false,
        }
    }
}

impl<Item, Error> Weight for Slot<Item, Error>
where
    Item: Weight,
{
    fn get_weight(&self) -> usize {
        match self {
            &Slot::Polling(_) | &Slot::Waiting(..) => 0,
            &Slot::Complete(ref v) => v.get_weight(),
        }
    }
}

/// Pending result from a cache lookup
pub struct MemoFuture<F>
where
    F: Filler,
    F::Key: Eq + Hash,
{
    cache: Asyncmemo<F>,
    key: F::Key,
    internal_future: Option<
        SharedAsyncmemoFuture<
            <<F::Value as IntoFuture>::Future as Future>::Item,
            <<F::Value as IntoFuture>::Future as Future>::Error,
        >,
    >,
}

impl<F> Debug for MemoFuture<F>
where
    F: Filler,
    F::Key: Eq + Hash + Debug,
    <<F as Filler>::Value as IntoFuture>::Future: Debug,
    <<F as Filler>::Value as IntoFuture>::Item: Debug,
    <<F as Filler>::Value as IntoFuture>::Error: Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("MemoFuture")
            .field("cache", &self.cache)
            .field("key", &self.key)
            .finish()
    }
}

fn wake_tasks(tasks: Vec<Task>) {
    for t in tasks {
        t.notify();
    }
}

impl<F> MemoFuture<F>
where
    F: Filler,
    F::Key: Eq + Hash + Weight + Clone,
    <F::Value as IntoFuture>::Item: Weight + Clone,
{
    // Return the current state of a slot, if present
    fn slot_present(&self) -> Option<FillerSlot<F>> {
        let mut hash = self.cache.inner.hash.lock().expect("locked poisoned");

        if let Some(entry) = hash.get_mut(&self.key) {
            match entry {
                // straightforward cache hit
                &mut Slot::Complete(ref val) => return Some(Slot::Complete(val.clone())),

                // Someone else is polling on the future, so just add ourselves to the set of
                // interested tasks and return.
                &mut Slot::Polling(ref mut wait) => {
                    wait.push(task::current());
                    return Some(Slot::Polling(Vec::new()));
                }

                // Last possibility: we're waiting and there's a future
                entry => {
                    let waiting = mem::replace(entry, Slot::Polling(Vec::new()));
                    assert!(waiting.is_waiting());
                    return Some(waiting);
                }
            }
        }

        // There's no existing entry, but we're about to make one so put in a placeholder
        // XXX use entry API?
        let _ = hash.insert(self.key.clone(), Slot::Polling(Vec::new()));
        None
    }

    fn slot_remove(&self) {
        let mut hash = self.cache.inner.hash.lock().expect("locked poisoned");

        if let Some(Slot::Polling(tasks)) = hash.remove(&self.key) {
            wake_tasks(tasks);
        }
    }

    fn slot_insert(&self, slot: FillerSlot<F>) {
        let mut hash = self.cache.inner.hash.lock().expect("locked poisoned");

        match hash.insert(self.key.clone(), slot) {
            Err((_k, _v)) => {
                // failed to insert for capacity reasons; remove entry we're not going to use
                // XXX retry once?
                hash.remove(&self.key);
            }
            Ok(Some(val @ Slot::Complete(_))) => {
                // If we just kicked out a complete value, put it back, since at best
                // we're replacing a complete value with another one (which should be
                // identical), but at worst we could be making it regress. This could only
                // happen if in-progress slot got evicted and the computation restarted.
                let _ = hash.insert(self.key.clone(), val);

                // trim cache if that made it oversized
                hash.trim();
            }
            Ok(Some(Slot::Polling(tasks))) => wake_tasks(tasks),
            Ok(Some(_)) | Ok(None) => (), // nothing (interesting) there
        }
    }

    fn poll_real_future(
        &mut self,
        mut real_future: SharedAsyncmemoFuture<<Self as Future>::Item, <Self as Future>::Error>,
    ) -> SharedAsyncmemoFuturePoll<<Self as Future>::Item, <Self as Future>::Error> {
        match real_future.poll() {
            Err(err) => {
                self.slot_remove();
                match err.lock().expect("error lock poisoned").take() {
                    Some(err) => SharedAsyncmemoFuturePoll::PollResult(Err(err)),
                    None => SharedAsyncmemoFuturePoll::MovedError,
                }
            }
            Ok(Async::NotReady) => {
                self.slot_insert(Slot::Waiting(real_future.clone()));
                self.internal_future = Some(real_future);
                SharedAsyncmemoFuturePoll::PollResult(Ok(Async::NotReady))
            }
            Ok(Async::Ready(val)) => {
                self.slot_insert(Slot::Complete((*val).clone()));
                SharedAsyncmemoFuturePoll::PollResult(Ok(Async::Ready((*val).clone())))
            }
        }
    }

    fn handle(&mut self) -> Poll<<Self as Future>::Item, <Self as Future>::Error> {
        // This is a 3-step process:
        // 1) Poll internal future if it is present. Internal future is present only if we have
        //    polled this MemoFuture before. Continue if the error was already moved away.
        // 2) Search for the future in the cache and poll. Continue if we can't find it or if the
        //    error was moved away.
        // 3) Get a future from the filler and poll it. Note that in that case error can't be
        //    moved away, because this future is not shared with any other MemoFuture.

        let internal_future = self.internal_future.take();
        if let Some(internal_future) = internal_future {
            if let SharedAsyncmemoFuturePoll::PollResult(poll) =
                self.poll_real_future(internal_future)
            {
                return poll;
            }
            // There was an Error, but another future has already replaced it with None.
            // In that case we want to start the future again.
        }

        // First check to see if we already have a slot for this key and process it accordingly.
        match self.slot_present() {
            None => (), // nothing there for this key
            Some(Slot::Complete(v)) => return Ok(Async::Ready(v)),
            Some(Slot::Polling(_)) => return Ok(Async::NotReady),
            Some(Slot::Waiting(fut)) => {
                if let SharedAsyncmemoFuturePoll::PollResult(poll) = self.poll_real_future(fut) {
                    return poll;
                }
                // There was an Error, but another future has already replaced it with None.
                // In that case we want to start the future again.
            }
        };

        // Slot was not present, but we have a placeholder now. Construct the Future and
        // start running it.

        let filler = self.cache
            .inner
            .filler
            .fill(&self.cache, &self.key)
            .into_future();

        let fut = wrap_filler_future(filler);
        if let SharedAsyncmemoFuturePoll::PollResult(poll) = self.poll_real_future(fut) {
            return poll;
        }
        panic!("internal error: just created future's error was already removed");
    }
}

impl<F> Future for MemoFuture<F>
where
    F: Filler,
    F::Key: Eq + Hash + Weight + Clone,
    <F::Value as IntoFuture>::Item: Weight + Clone,
{
    type Item = <F::Value as IntoFuture>::Item;
    type Error = <F::Value as IntoFuture>::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.handle()
    }
}

impl<F> Asyncmemo<F>
where
    F: Filler,
    F::Key: Hash + Eq + Weight,
    <F::Value as IntoFuture>::Item: Weight,
{
    /// Construct a new bounded cache. It enforces two distinct limits:
    /// - entrylimit - the max number of entries
    /// - weightlimit - the max abstract "weight" of the entries (both keys and values)
    ///
    /// Weight is typically memory use.
    pub fn with_limits(fill: F, entrylimit: usize, weightlimit: usize) -> Self {
        assert!(entrylimit > 0);
        assert!(weightlimit > 0);

        let inner = AsyncmemoInner {
            hash: Mutex::new(BoundedHash::new(entrylimit, weightlimit)),
            filler: fill,
        };

        Asyncmemo {
            inner: Arc::new(inner),
        }
    }

    /// Construct an unbounded cache.
    ///
    /// This is pretty dangerous for any non-toy use.
    pub fn new_unbounded(fill: F) -> Self {
        Self::with_limits(fill, usize::MAX, usize::MAX)
    }

    /// Look up a result for a particular key/arg
    ///
    /// The future will either complete immediately if the result is already
    /// known, or will wait for an in-progress result (perhaps failing), or
    /// initiate new process to generate a result.
    ///
    /// This only caches successful results - it does not cache errors as a
    /// negative cache.
    pub fn get<K: Into<F::Key>>(&self, key: K) -> MemoFuture<F> {
        MemoFuture {
            cache: self.clone(),
            key: key.into(),
            internal_future: None,
        }
    }

    /// Invalidate a specific key
    pub fn invalidate<K: Into<F::Key>>(&self, key: K) {
        let mut locked = self.inner.hash.lock().expect("lock poison");
        let key = key.into();
        let _ = locked.remove(&key);
    }

    /// Reset the cache. This removes all results (complete and in-progress) from the cache.
    /// This drops the futures of in-progress entries, which should propagate cancellation
    /// if necessary.
    pub fn clear(&self) {
        let mut locked = self.inner.hash.lock().expect("lock poison");

        locked.clear()
    }

    /// Trim cache size to limits.
    pub fn trim(&self) {
        let mut locked = self.inner.hash.lock().expect("lock poison");

        locked.trim_entries(0);
        locked.trim_weight(0);
    }

    /// Return number of entries in cache.
    pub fn len(&self) -> usize {
        let hash = self.inner.hash.lock().expect("lock poison");
        hash.len()
    }

    /// Return current "weight" of the cache entries
    pub fn total_weight(&self) -> usize {
        let hash = self.inner.hash.lock().expect("lock poison");
        hash.total_weight()
    }

    /// Return true if cache is empty.
    pub fn is_empty(&self) -> bool {
        let hash = self.inner.hash.lock().expect("lock poison");
        hash.is_empty()
    }
}

impl<F> Clone for Asyncmemo<F>
where
    F: Filler,
    F::Key: Eq + Hash,
{
    fn clone(&self) -> Self {
        Asyncmemo {
            inner: self.inner.clone(),
        }
    }
}
