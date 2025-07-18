use std::{
    error::Error,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{poll::PollHandle, thread_pool::ThreadPool};
use mio::{Events, event::Event};

const EVENTS_CAPACITY: usize = 1024;
const POLL_TIMEOUT_MS: u64 = 150;

pub struct Reactor {
    pub(crate) poll_handle: PollHandle,
    events: Arc<RwLock<Events>>,
    pool: ThreadPool,
    running: AtomicBool,
}

impl Reactor {
    pub fn new(pool_size: usize) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            poll_handle: PollHandle::new()?,
            events: Arc::new(RwLock::new(Events::with_capacity(EVENTS_CAPACITY))),
            pool: ThreadPool::new(pool_size),
            running: AtomicBool::new(false),
        })
    }

    pub fn run(&self) -> Result<(), Box<dyn Error>> {
        self.running.store(true, Ordering::SeqCst);

        while self.running.load(Ordering::SeqCst) {
            println!("{}", self.running.load(Ordering::SeqCst));
            let _ = self.poll_handle.poll(
                &mut self.events.write().unwrap(),
                Some(Duration::from_millis(POLL_TIMEOUT_MS)),
            )?;

            for event in self.events.read().unwrap().iter() {
                self.dispatch_event(event.clone())?;
            }
        }
        Ok(())
    }

    pub fn get_shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            running: &self.running,
            poll_handle: &self.poll_handle,
        }
    }

    pub fn dispatch_event(&self, event: Event) -> Result<(), Box<dyn Error>> {
        let token = event.token();

        let registry = self.poll_handle.get_registery();

        self.pool.exec(move || {
            let registry = registry.read().unwrap();
            let entry = registry.get(&token);
            let e = entry.is_some();

            if let Some(entry) = entry {
                if (entry.interest.is_readable() && event.is_readable())
                    || (entry.interest.is_writable() && event.is_writable())
                {
                    entry.handler.handle_event(&event);
                }
            }
        })
    }

    pub fn get_events(&self) -> Arc<RwLock<Events>> {
        self.events.clone()
    }
}

pub struct ShutdownHandle<'a> {
    running: &'a AtomicBool,
    poll_handle: &'a PollHandle,
}

impl<'a> ShutdownHandle<'a> {
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.poll_handle.wake().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::*;
    use mio::{Interest, Token};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    #[derive(Clone)]
    struct TestHandler {
        counter: Arc<Mutex<usize>>,
        condition: Arc<Condvar>,
    }

    impl EventHandler for TestHandler {
        fn handle_event(&self, _event: &Event) {
            let mut count = self.counter.lock().unwrap();
            *count += 1;
            self.condition.notify_one();
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_reactor_start_stop() {
        let reactor = Arc::new(Reactor::new(4).unwrap());
        let shutdown_handle = reactor.get_shutdown_handle();

        let reactor_clone = Arc::clone(&reactor);
        let handle = std::thread::spawn(move || {
            reactor_clone.run().unwrap();
        });

        std::thread::sleep(Duration::from_millis(100));

        shutdown_handle.shutdown();

        handle.join().unwrap();
    }

    #[test]
    fn test_with_pipe() -> std::io::Result<()> {
        use mio::net::UnixStream;

        let reactor = Arc::new(Reactor::new(2).unwrap());
        let counter = Arc::new(Mutex::new(0));
        let condition = Arc::new(Condvar::new());

        let (mut stream1, mut stream2) = UnixStream::pair()?;

        let handler = TestHandler {
            counter: Arc::clone(&counter),
            condition: Arc::clone(&condition),
        };

        let token = Token(1);

        reactor
            .poll_handle
            .register(&mut stream1, token, Interest::READABLE, handler)
            .unwrap();

        let reactor_clone = Arc::clone(&reactor);
        let handle = std::thread::spawn(move || {
            // Poll once
            let events_result = {
                let mut events = reactor_clone.events.write().unwrap();
                reactor_clone
                    .poll_handle
                    .poll(&mut *events, Some(Duration::from_millis(100)))
            };

            if let Ok(_) = events_result {
                let events = reactor_clone.events.read().unwrap();
                for event in events.iter() {
                    let _ = reactor_clone.dispatch_event(event.clone());
                }
            }
        });

        std::io::Write::write_all(&mut stream2, b"test data")?;

        handle.join().unwrap();

        let count = counter.lock().unwrap();
        let result = condition
            .wait_timeout(count, Duration::from_millis(500))
            .unwrap();

        if !result.1.timed_out() {
            assert_eq!(*result.0, 1);
        }

        Ok(())
    }
}
