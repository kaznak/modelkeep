//! Shared, interruptible single-flight coordination.
//!
//! An acquisition runs on its own OS thread for the same reason management jobs
//! do (Issue 0056): it must outlive the request that started it, and dropping
//! the Tokio runtime must never wait for a multi-hour upstream transfer.
//! Waiters attach to the running flight and may leave on a deadline; the flight
//! itself runs to completion whether or not anyone is still waiting, so a client
//! that gives up never discards transferred bytes and its retry joins the same
//! flight instead of starting a second download.

use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    panic::AssertUnwindSafe,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Instant,
};

/// How many progress events one flight retains for waiters that have not
/// drained them yet.
///
/// Progress is advisory. A waiter that falls this far behind skips the oldest
/// events rather than making the acquisition thread block or letting the flight
/// grow without bound.
const PROGRESS_HISTORY: usize = 64;

/// What a waiter learned by joining a flight.
#[derive(Debug)]
pub enum Joined<V, E> {
    /// The acquisition finished while this waiter was attached.
    Completed(Result<V, E>),
    /// The deadline elapsed first. The acquisition is still running, and a
    /// later waiter joins this same flight rather than starting a second one.
    Pending,
    /// The acquisition thread ended without producing a result, which can only
    /// happen if it panicked. Callers must report an internal failure, never a
    /// miss.
    Abandoned,
}

enum Completion<V, E> {
    Running,
    Done(Result<V, E>),
    Abandoned,
}

struct FlightState<V, E, P> {
    completion: Completion<V, E>,
    progress: VecDeque<(u64, P)>,
    next_sequence: u64,
}

struct Flight<V, E, P> {
    state: Mutex<FlightState<V, E, P>>,
    changed: Condvar,
}

impl<V, E, P> Flight<V, E, P>
where
    V: Clone,
    E: Clone,
    P: Clone,
{
    fn new() -> Self {
        Self {
            state: Mutex::new(FlightState {
                completion: Completion::Running,
                progress: VecDeque::new(),
                next_sequence: 0,
            }),
            changed: Condvar::new(),
        }
    }

    fn publish(&self, event: P) {
        let mut state = self.state.lock().expect("flight state lock poisoned");
        let sequence = state.next_sequence;
        state.next_sequence += 1;
        state.progress.push_back((sequence, event));
        while state.progress.len() > PROGRESS_HISTORY {
            state.progress.pop_front();
        }
        drop(state);
        self.changed.notify_all();
    }

    fn finish(&self, outcome: Option<Result<V, E>>) {
        let mut state = self.state.lock().expect("flight state lock poisoned");
        state.completion = match outcome {
            Some(result) => Completion::Done(result),
            None => Completion::Abandoned,
        };
        drop(state);
        self.changed.notify_all();
    }

    /// Waits for this flight, relaying progress to `observe` as it appears.
    ///
    /// Progress is relayed by the waiter rather than pushed by the acquisition
    /// thread, so a caller can pass a borrowed observer without the acquisition
    /// outliving it.
    fn wait(&self, deadline: Option<Instant>, observe: &dyn Fn(P)) -> Joined<V, E> {
        // Start one event behind so a late waiter immediately learns the
        // current phase and byte count instead of replaying the whole history.
        let mut cursor = {
            let state = self.state.lock().expect("flight state lock poisoned");
            state.next_sequence.saturating_sub(1)
        };
        let mut expired = false;
        loop {
            let state = self.state.lock().expect("flight state lock poisoned");
            let mut pending = Vec::new();
            for (sequence, event) in state.progress.iter() {
                if *sequence >= cursor {
                    cursor = sequence + 1;
                    pending.push(event.clone());
                }
            }
            let completion = match &state.completion {
                Completion::Running => None,
                Completion::Done(result) => Some(Joined::Completed(result.clone())),
                Completion::Abandoned => Some(Joined::Abandoned),
            };
            if completion.is_none() && pending.is_empty() {
                if expired {
                    return Joined::Pending;
                }
                match deadline {
                    None => {
                        let _released = self
                            .changed
                            .wait(state)
                            .expect("flight state lock poisoned");
                    }
                    Some(deadline) => {
                        let Some(remaining) = deadline.checked_duration_since(Instant::now())
                        else {
                            return Joined::Pending;
                        };
                        let (_released, timeout) = self
                            .changed
                            .wait_timeout(state, remaining)
                            .expect("flight state lock poisoned");
                        expired = timeout.timed_out();
                    }
                }
                continue;
            }
            // The observer is caller code; it never runs under the flight lock.
            drop(state);
            for event in pending {
                observe(event);
            }
            if let Some(completion) = completion {
                return completion;
            }
        }
    }
}

type Flights<K, V, E, P> = Mutex<HashMap<K, Arc<Flight<V, E, P>>>>;

struct Registry<K, V, E, P> {
    flights: Flights<K, V, E, P>,
}

pub struct SingleFlight<K, V, E, P> {
    registry: Arc<Registry<K, V, E, P>>,
}

impl<K, V, E, P> Default for SingleFlight<K, V, E, P> {
    fn default() -> Self {
        Self {
            registry: Arc::new(Registry {
                flights: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl<K, V, E, P> SingleFlight<K, V, E, P>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + 'static,
    E: Clone + Send + 'static,
    P: Clone + Send + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the flight for `key`, or attaches to the one already running.
    ///
    /// `fetch` runs on a dedicated thread and receives a progress sink. With
    /// `deadline: None` the caller waits for the result, which is what a
    /// management job needs so its record reaches a terminal state. With a
    /// deadline the caller may return [`Joined::Pending`] while the flight keeps
    /// running.
    pub fn join<F>(
        &self,
        key: K,
        fetch: F,
        deadline: Option<Instant>,
        observe: &dyn Fn(P),
    ) -> Joined<V, E>
    where
        F: FnOnce(&(dyn Fn(P) + Send + Sync)) -> Result<V, E> + Send + 'static,
    {
        let flight = {
            let mut flights = self
                .registry
                .flights
                .lock()
                .expect("single-flight lock poisoned");
            match flights.get(&key).map(Arc::clone) {
                Some(flight) => flight,
                None => {
                    let flight = Arc::new(Flight::new());
                    flights.insert(key.clone(), Arc::clone(&flight));
                    let registry = Arc::clone(&self.registry);
                    let worker = Arc::clone(&flight);
                    let worker_key = key.clone();
                    let spawned = thread::Builder::new()
                        .name("modelkeep-acquisition".into())
                        .spawn(move || {
                            let sink = {
                                let flight = Arc::clone(&worker);
                                move |event: P| flight.publish(event)
                            };
                            let outcome =
                                std::panic::catch_unwind(AssertUnwindSafe(|| fetch(&sink)));
                            // Retiring the key before publishing the result, under
                            // the registry lock, keeps a caller that arrives now
                            // from joining a finished flight: it either shares the
                            // still-running one or starts a fresh attempt.
                            let mut flights = registry
                                .flights
                                .lock()
                                .expect("single-flight lock poisoned");
                            flights.remove(&worker_key);
                            worker.finish(outcome.ok());
                        });
                    if spawned.is_err() {
                        flights.remove(&key);
                        drop(flights);
                        panic!("modelkeep could not start an acquisition thread");
                    }
                    flight
                }
            }
        };
        flight.wait(deadline, observe)
    }
}

#[cfg(test)]
mod tests {
    use super::{Joined, SingleFlight};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc, Mutex,
        },
        thread,
        time::{Duration, Instant},
    };

    fn completed<V, E>(joined: Joined<V, E>) -> Result<V, E> {
        match joined {
            Joined::Completed(result) => result,
            Joined::Pending => panic!("flight was still pending"),
            Joined::Abandoned => panic!("flight was abandoned"),
        }
    }

    #[test]
    fn concurrent_callers_share_one_result() {
        let flights = Arc::new(SingleFlight::<&str, usize, &str, ()>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let flights = Arc::clone(&flights);
            let calls = Arc::clone(&calls);
            threads.push(thread::spawn(move || {
                completed(flights.join(
                    "model@commit/file",
                    move |_progress| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(10));
                        Ok(42)
                    },
                    None,
                    &|()| {},
                ))
                .unwrap()
            }));
        }
        for thread in threads {
            assert_eq!(thread.join().unwrap(), 42);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failures_are_propagated_and_next_call_can_retry() {
        let flights = SingleFlight::<&str, usize, &str, ()>::new();
        assert_eq!(
            completed(flights.join("key", |_progress| Err("upstream"), None, &|()| {})),
            Err("upstream")
        );
        assert_eq!(
            completed(flights.join("key", |_progress| Ok(7), None, &|()| {})),
            Ok(7)
        );
    }

    #[test]
    fn a_waiter_can_leave_on_its_deadline_while_the_flight_runs_to_completion() {
        let flights = Arc::new(SingleFlight::<&str, usize, &str, ()>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, released) = mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        let started = {
            let calls = Arc::clone(&calls);
            let released = Arc::clone(&released);
            move |_progress: &(dyn Fn(()) + Send + Sync)| {
                calls.fetch_add(1, Ordering::SeqCst);
                released.lock().unwrap().recv().unwrap();
                Ok(11)
            }
        };
        let abandoned = flights.join(
            "slow",
            started,
            Some(Instant::now() + Duration::from_millis(50)),
            &|()| {},
        );
        assert!(matches!(abandoned, Joined::Pending));

        // A retry joins the same flight instead of starting a second fetch.
        let joiner = {
            let flights = Arc::clone(&flights);
            thread::spawn(move || {
                completed(flights.join(
                    "slow",
                    |_progress| panic!("a second acquisition must not start"),
                    None,
                    &|()| {},
                ))
            })
        };
        thread::sleep(Duration::from_millis(50));
        release.send(()).unwrap();
        assert_eq!(joiner.join().unwrap(), Ok(11));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn waiters_observe_progress_published_by_the_flight() {
        let flights = SingleFlight::<&str, usize, &str, u64>::new();
        let seen = Mutex::new(Vec::new());
        let result = flights.join(
            "progress",
            |progress| {
                progress(1);
                progress(2);
                progress(3);
                Ok(0)
            },
            None,
            &|event| seen.lock().unwrap().push(event),
        );
        assert_eq!(completed(result), Ok(0));
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "progress must be relayed in order: {seen:?}"
        );
        assert_eq!(seen.last().copied(), Some(3));
    }

    #[test]
    fn a_panicking_acquisition_is_reported_rather_than_hanging_its_waiters() {
        let flights = Arc::new(SingleFlight::<&str, usize, &str, ()>::new());
        let joiner = {
            let flights = Arc::clone(&flights);
            thread::spawn(move || {
                flights.join(
                    "panic",
                    |_progress| {
                        thread::sleep(Duration::from_millis(30));
                        panic!("acquisition bug")
                    },
                    None,
                    &|()| {},
                )
            })
        };
        assert!(matches!(joiner.join().unwrap(), Joined::Abandoned));
    }
}
