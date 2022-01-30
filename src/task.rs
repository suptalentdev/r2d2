use std::collections::BinaryHeap;
use std::cmp::{PartialOrd, Ord, PartialEq, Eq, Ordering};
use std::sync::{Arc, Mutex, Condvar};
use std::thread::Thread;
use std::thunk::Thunk;
use std::time::Duration;

use time;

enum JobType {
    Once(Thunk),
    FixedRate {
        f: Box<FnMut() + Send>,
        rate: Duration,
    },
}

struct Job {
    type_: JobType,
    time: u64,
}

impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Job) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Job {
    fn cmp(&self, other: &Job) -> Ordering {
        // reverse because BinaryHeap's a max heap
        self.time.cmp(&other.time).reverse()
    }
}

impl PartialEq for Job {
    fn eq(&self, other: &Job) -> bool {
        self.time == other.time
    }
}

impl Eq for Job {}

struct InnerPool {
    queue: BinaryHeap<Job>,
    shutdown: bool,
}

struct SharedPool {
    inner: Mutex<InnerPool>,
    cvar: Condvar,
}

impl SharedPool {
    fn run(&self, job: Job) {
        let mut inner = self.inner.lock().unwrap();

        // Calls from the pool itself will never hit this, but calls from workers might
        if inner.shutdown {
            return;
        }

        match inner.queue.peek() {
            None => self.cvar.notify_all(),
            Some(e) if e.time > job.time => self.cvar.notify_all(),
            _ => {}
        };
        inner.queue.push(job);
    }
}

/// A fixed-size thread pool which allows jobs to be scheduled in the future.
///
/// When the pool falls out of scope, all pending tasks will be executed, after
/// which the worker threads will shut down.
pub struct ScheduledThreadPool {
    shared: Arc<SharedPool>,
}

impl Drop for ScheduledThreadPool {
    fn drop(&mut self) {
        self.shared.inner.lock().unwrap().shutdown = true;
        self.shared.cvar.notify_all();
    }
}

impl ScheduledThreadPool {
    /// Creates a new `ScheduledThreadPool` with the specified number of threads.
    ///
    /// # Panics
    ///
    /// Panics if `size` is 0.
    pub fn new(size: usize) -> ScheduledThreadPool {
        assert!(size > 0, "size must be positive");

        let inner = InnerPool {
            queue: BinaryHeap::new(),
            shutdown: false,
        };

        let shared = SharedPool {
            inner: Mutex::new(inner),
            cvar: Condvar::new(),
        };

        let pool = ScheduledThreadPool {
            shared: Arc::new(shared),
        };

        for _ in (0..size) {
            let mut worker = Worker {
                shared: pool.shared.clone(),
            };

            Thread::spawn(move || worker.run());
        }

        pool
    }

    /// Asynchronously executes `job` with no added delay.
    pub fn run<F>(&self, job: F) where F: FnOnce() + Send {
        self.run_after(Duration::zero(), job)
    }

    /// Asynchronously executes `job` after the specified delay.
    pub fn run_after<F>(&self, dur: Duration, job: F) where F: FnOnce() + Send {
        let job = Job {
            type_: JobType::Once(Thunk::new(job)),
            time: (time::precise_time_ns() as i64 + dur.num_nanoseconds().unwrap()) as u64,
        };
        self.shared.run(job)
    }

    /// Asynchronously executes `job` repeatedly at the specified rate.
    ///
    /// If the job panics, it will no longer be executed. When the pool is
    /// destroyed, the job will no longer be rescheduled for execution, but any
    /// pending execution of the job will be handled as a normal job would.
    pub fn run_at_fixed_rate<F>(&self, rate: Duration, f: F) where F: FnMut() + Send {
        let job = Job {
            type_: JobType::FixedRate { f: Box::new(f), rate: rate },
            time: (time::precise_time_ns() as i64 + rate.num_nanoseconds().unwrap()) as u64,
        };
        self.shared.run(job)
    }

    /// Consumes the `ScheduledThreadPool`, canceling any pending jobs.
    ///
    /// Currently running jobs will continue to run to completion.
    pub fn shutdown_now(self) {
        self.shared.inner.lock().unwrap().queue.clear();
    }
}

struct Worker {
    shared: Arc<SharedPool>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Start up a new worker if this one's going away due to a panic from a job
        if Thread::panicking() {
            let mut worker = Worker {
                shared: self.shared.clone(),
            };
            Thread::spawn(move || worker.run());
        }
    }
}

impl Worker {
    fn run(&mut self) {
        loop {
            match self.get_job() {
                Some(job) => self.run_job(job),
                None => break,
            }
        }
    }

    fn get_job(&self) -> Option<Job> {
        enum Need {
            Wait,
            WaitTimeout(Duration),
        }

        let mut inner = self.shared.inner.lock().unwrap();
        loop {
            let now = time::precise_time_ns();

            let need = match inner.queue.peek() {
                None if inner.shutdown => return None,
                None => Need::Wait,
                Some(e) if e.time <= now => break,
                Some(e) => Need::WaitTimeout(Duration::nanoseconds(e.time as i64 - now as i64)),
            };

            inner = match need {
                Need::Wait => self.shared.cvar.wait(inner).unwrap(),
                Need::WaitTimeout(t) => self.shared.cvar.wait_timeout(inner, t).unwrap().0,
            };
        }

        Some(inner.queue.pop().unwrap())
    }

    fn run_job(&self, job: Job) {
        match job.type_ {
            JobType::Once(f) => f.invoke(()),
            JobType::FixedRate { mut f, rate } => {
                f();
                let new_job = Job {
                    type_: JobType::FixedRate { f: f, rate: rate },
                    time: (job.time as i64 + rate.num_nanoseconds().unwrap()) as u64,
                };
                self.shared.run(new_job)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::iter::AdditiveIterator;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    use super::ScheduledThreadPool;

    const TEST_TASKS: usize = 4;

    #[test]
    fn test_works() {
        let pool = ScheduledThreadPool::new(TEST_TASKS);

        let (tx, rx) = channel();
        for _ in range(0, TEST_TASKS) {
            let tx = tx.clone();
            pool.run(move|| {
                tx.send(1us).unwrap();
            });
        }

        assert_eq!(rx.iter().take(TEST_TASKS).sum(), TEST_TASKS);
    }

    #[test]
    #[should_fail(expected = "size must be positive")]
    fn test_zero_tasks_panic() {
        ScheduledThreadPool::new(0);
    }

    #[test]
    fn test_recovery_from_subtask_panic() {
        let pool = ScheduledThreadPool::new(TEST_TASKS);

        // Panic all the existing threads.
        let waiter = Arc::new(Barrier::new(TEST_TASKS as usize));
        for _ in range(0, TEST_TASKS) {
            let waiter = waiter.clone();
            pool.run(move || -> () {
                waiter.wait();
                panic!();
            });
        }

        // Ensure new threads were spawned to compensate.
        let (tx, rx) = channel();
        let waiter = Arc::new(Barrier::new(TEST_TASKS as usize));
        for _ in range(0, TEST_TASKS) {
            let tx = tx.clone();
            let waiter = waiter.clone();
            pool.run(move || {
                waiter.wait();
                tx.send(1us).unwrap();
            });
        }

        assert_eq!(rx.iter().take(TEST_TASKS).sum(), TEST_TASKS);
    }

    #[test]
    fn test_run_after() {
        let pool = ScheduledThreadPool::new(TEST_TASKS);
        let (tx, rx) = channel();

        let tx1 = tx.clone();
        pool.run_after(Duration::seconds(1), move || tx1.send(1us).unwrap());
        pool.run_after(Duration::milliseconds(500), move || tx.send(2us).unwrap());

        assert_eq!(2, rx.recv().unwrap());
        assert_eq!(1, rx.recv().unwrap());
    }

    #[test]
    fn test_jobs_complete_after_drop() {
        let pool = ScheduledThreadPool::new(TEST_TASKS);
        let (tx, rx) = channel();

        let tx1 = tx.clone();
        pool.run_after(Duration::seconds(1), move || tx1.send(1us).unwrap());
        pool.run_after(Duration::milliseconds(500), move || tx.send(2us).unwrap());

        drop(pool);

        assert_eq!(2, rx.recv().unwrap());
        assert_eq!(1, rx.recv().unwrap());
    }

    #[test]
    fn test_fixed_delay_jobs_stop_after_drop() {
        let pool = Arc::new(ScheduledThreadPool::new(TEST_TASKS));
        let (tx, rx) = channel();
        let (tx2, rx2) = channel();

        let mut pool2 = Some(pool.clone());
        let mut i = 0i32;
        pool.run_at_fixed_rate(Duration::milliseconds(500), move || {
            i += 1;
            tx.send(i).unwrap();
            rx2.recv().unwrap();
            if i == 2 {
                drop(pool2.take().unwrap());
            }
        });
        drop(pool);

        assert_eq!(Ok(1), rx.recv());
        tx2.send(()).unwrap();
        assert_eq!(Ok(2), rx.recv());
        tx2.send(()).unwrap();
        assert!(rx.recv().is_err());
    }
}
