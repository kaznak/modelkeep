use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Condvar, Mutex},
};

struct Flight<V, E> {
    result: Mutex<Option<Result<V, E>>>,
    completed: Condvar,
}

pub struct SingleFlight<K, V, E> {
    flights: Mutex<HashMap<K, Arc<Flight<V, E>>>>,
}

impl<K, V, E> SingleFlight<K, V, E>
where
    K: Eq + Hash + Clone,
    V: Clone,
    E: Clone,
{
    pub fn new() -> Self {
        Self {
            flights: Mutex::new(HashMap::new()),
        }
    }

    pub fn run<F>(&self, key: K, fetch: F) -> Result<V, E>
    where
        F: FnOnce() -> Result<V, E>,
    {
        let (flight, leader) = {
            let mut flights = self.flights.lock().expect("single-flight lock poisoned");
            if let Some(flight) = flights.get(&key) {
                (Arc::clone(flight), false)
            } else {
                let flight = Arc::new(Flight {
                    result: Mutex::new(None),
                    completed: Condvar::new(),
                });
                flights.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };

        if leader {
            let result = fetch();
            *flight.result.lock().expect("flight lock poisoned") = Some(result.clone());
            flight.completed.notify_all();
            self.flights
                .lock()
                .expect("single-flight lock poisoned")
                .remove(&key);
            result
        } else {
            let mut result = flight.result.lock().expect("flight lock poisoned");
            while result.is_none() {
                result = flight.completed.wait(result).expect("flight lock poisoned");
            }
            result.clone().expect("completed flight has a result")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SingleFlight;
    use std::{sync::Arc, thread, time::Duration};

    #[test]
    fn concurrent_callers_share_one_result() {
        let flights = Arc::new(SingleFlight::<&str, usize, &str>::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let flights = Arc::clone(&flights);
            let calls = Arc::clone(&calls);
            threads.push(thread::spawn(move || {
                flights
                    .run("model@commit/file", || {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(10));
                        Ok(42)
                    })
                    .unwrap()
            }));
        }
        for thread in threads {
            assert_eq!(thread.join().unwrap(), 42);
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn failures_are_propagated_and_next_call_can_retry() {
        let flights = SingleFlight::<&str, usize, &str>::new();
        assert_eq!(flights.run("key", || Err("upstream")), Err("upstream"));
        assert_eq!(flights.run("key", || Ok(7)), Ok(7));
    }
}
