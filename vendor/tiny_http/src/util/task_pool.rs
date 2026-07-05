use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Manages a collection of threads.
///
/// A new thread is created every time all the existing threads are full.
/// Any idle thread will automatically die after a few seconds.
pub struct TaskPool {
    sharing: Arc<Sharing>,
}

struct Sharing {
    // list of the tasks to be done by worker threads
    todo: Mutex<VecDeque<Box<dyn FnMut() + Send>>>,

    // condvar that will be notified whenever a task is added to `todo`
    condvar: Condvar,

    // number of total worker threads running
    active_tasks: AtomicUsize,

    // number of idle worker threads
    waiting_tasks: AtomicUsize,
}

/// Minimum number of active threads.
static MIN_THREADS: usize = 4;

/// LOCAL PATCH (yt-websub): maximum number of worker threads. Upstream had no
/// ceiling — it spawned a fresh OS thread for every connection whenever no
/// worker was idle, using a panicking `std::thread::spawn`. A flood of stalled
/// connections thus climbed until the process/pid limit (systemd `TasksMax=32`)
/// and the next spawn PANICKED, which `panic=abort` escalated to a whole-process
/// SIGABRT. We now cap the pool below `TasksMax` (leaving headroom for the app's
/// own accept workers, the renewal thread, and libc); beyond the cap, tasks
/// queue until a worker frees. Socket read timeouts (see connection.rs) reap
/// stalled connections so the pool drains. Paired with a non-panicking spawn
/// below, hitting a resource ceiling drops one connection instead of the server.
static MAX_THREADS: usize = 20;

struct Registration<'a> {
    nb: &'a AtomicUsize,
}

impl<'a> Registration<'a> {
    fn new(nb: &'a AtomicUsize) -> Registration<'a> {
        nb.fetch_add(1, Ordering::Release);
        Registration { nb }
    }
}

impl<'a> Drop for Registration<'a> {
    fn drop(&mut self) {
        self.nb.fetch_sub(1, Ordering::Release);
    }
}

impl TaskPool {
    pub fn new() -> TaskPool {
        let pool = TaskPool {
            sharing: Arc::new(Sharing {
                todo: Mutex::new(VecDeque::new()),
                condvar: Condvar::new(),
                active_tasks: AtomicUsize::new(0),
                waiting_tasks: AtomicUsize::new(0),
            }),
        };

        for _ in 0..MIN_THREADS {
            pool.add_thread(None)
        }

        pool
    }

    /// Executes a function in a thread.
    /// If no thread is available, spawns a new one.
    pub fn spawn(&self, code: Box<dyn FnMut() + Send>) {
        let mut queue = self.sharing.todo.lock().unwrap();

        // LOCAL PATCH (yt-websub): only spawn a new worker if none is idle AND we
        // are below the thread ceiling; otherwise queue the task for an existing
        // worker. `active_tasks` is incremented inside the worker so it can lag
        // slightly under a burst, but the non-panicking add_thread makes any
        // overshoot harmless rather than fatal.
        if self.sharing.waiting_tasks.load(Ordering::Acquire) == 0
            && self.sharing.active_tasks.load(Ordering::Acquire) < MAX_THREADS
        {
            self.add_thread(Some(code));
        } else {
            queue.push_back(code);
            self.sharing.condvar.notify_one();
        }
    }

    fn add_thread(&self, initial_fn: Option<Box<dyn FnMut() + Send>>) {
        let sharing = self.sharing.clone();

        // LOCAL PATCH (yt-websub): use a fallible Builder::spawn instead of the
        // panicking thread::spawn. If the OS refuses a new thread (e.g. the
        // systemd TasksMax pid ceiling under a connection flood), we log and drop
        // the closure — which closes that one pending connection — instead of
        // panicking and (under panic=abort) killing the entire process.
        let spawn_result = thread::Builder::new().spawn(move || {
            let sharing = sharing;
            let _active_guard = Registration::new(&sharing.active_tasks);

            if let Some(mut f) = initial_fn {
                f();
            }

            loop {
                let mut task: Box<dyn FnMut() + Send> = {
                    let mut todo = sharing.todo.lock().unwrap();

                    let task;
                    loop {
                        if let Some(poped_task) = todo.pop_front() {
                            task = poped_task;
                            break;
                        }
                        let _waiting_guard = Registration::new(&sharing.waiting_tasks);

                        let received =
                            if sharing.active_tasks.load(Ordering::Acquire) <= MIN_THREADS {
                                todo = sharing.condvar.wait(todo).unwrap();
                                true
                            } else {
                                let (new_lock, waitres) = sharing
                                    .condvar
                                    .wait_timeout(todo, Duration::from_millis(5000))
                                    .unwrap();
                                todo = new_lock;
                                !waitres.timed_out()
                            };

                        if !received && todo.is_empty() {
                            return;
                        }
                    }

                    task
                };

                task();
            }
        });

        if let Err(e) = spawn_result {
            log::error!("TaskPool: OS refused a new worker thread: {}", e);
        }
    }
}

impl Drop for TaskPool {
    fn drop(&mut self) {
        self.sharing
            .active_tasks
            .store(999_999_999, Ordering::Release);
        self.sharing.condvar.notify_all();
    }
}
