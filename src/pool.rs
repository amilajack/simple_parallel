use std::collections::BinaryHeap;
use std::iter::IntoIterator;
use std::{marker, mem};
use std::sync::{mpsc, atomic};
use std::thread;
use std::thunk::Invoke;

type JobInner<'b> =  Box<for<'a> Invoke<&'a [mpsc::Sender<Work>], ()> + Send + 'b>;
struct Job {
    func: JobInner<'static>,
}

/// A thread pool.
///
/// This pool allows one to spawn several threads in one go, and then
/// execute any number of "short-lifetime" jobs on those threads,
/// without having to pay the thread spawning cost, or risk exhausting
/// system resources.
///
/// The pool currently consists of some number of worker threads
/// (dynamic, chosen at creation time) along with a single supervisor
/// thread. The synchronisation overhead is currently very large.
///
/// # "Short-lifetime"?
///
/// Jobs submitted to this pool can have any lifetime at all, that is,
/// the closures passed in (and elements of iterators used, etc.) can
/// have borrows pointing into arbitrary stack frames, even stack
/// frames that don't outlive the pool itself. This differs to
/// [`thread_pool::ScopedPool`](http://doc.rust-lang.org/threadpool/threadpool/struct.ScopedPool.html),
/// where the jobs must outlive the pool.
///
/// This extra flexibility is achieved with careful unsafe code, by
/// exposing an API that is a generalised version of
/// `std::thread::scoped`: at the lowest-level a submitted job returns
/// a `JobHandle` token that ensures that job is finished before any
/// data the job might reference is invalidated (i.e. manages the
/// lifetimes). Higher-level functions will usually wrap or otherwise
/// hide the handle.
///
/// However, this comes at a cost: for easy of implementation `Pool`
/// currently only exposes "batch" jobs like `for_` and `map` and
/// these jobs take control of the whole pool. That is, one cannot
/// easily incrementally submit arbitrary closures to execute on this
/// thread pool, which is functionality that `threadpool::ScopedPool`
/// offers.
///
/// # Example
///
/// ```rust
/// use simple_parallel::Pool;
///
/// // a function that takes some arbitrary pool and uses the pool to
/// // manipulate data in its own stack frame.
/// fn do_work(pool: &mut Pool) {
///     let mut v = [0; 8];
///     // set each element, in parallel
///     pool.for_(&mut v, |element| *element = 3);
///
///     let w = [2, 0, 1, 5, 0, 3, 0, 3];
///
///     // add the two arrays, in parallel
///     let f = |(x, y): (&i32, &i32)| *x + *y;
///     let z: Vec<_> = pool.map(v.iter().zip(w.iter()), &f).collect();
///
///     assert_eq!(z, &[5, 3, 4, 8, 3, 6, 3, 6]);
/// }
///
/// let mut pool = Pool::new(4);
/// do_work(&mut pool);
/// ```
pub struct Pool {
    job_queue: mpsc::Sender<Option<Job>>,
    job_finished: mpsc::Receiver<Result<(), ()>>,
    n_threads: usize,
}
#[derive(Copy)]
struct WorkerId { n: usize }

type WorkInner<'a> = &'a mut (FnMut(WorkerId) + Send + 'a);
struct Work {
    func: WorkInner<'static>
}

/// A token representing a job submitted to the thread pool.
///
/// This ensures that a job is finished before borrowed resources in
/// the job (and the pool itself) are invalidated.
///
/// If the job panics, this handle will ensure the main thread also
/// panics (either via `wait` or in the destructor).
pub struct JobHandle<'pool, 'f> {
    pool: &'pool mut Pool,
    wait: bool,
    _funcs: marker::PhantomData<&'f ()>,
}
impl<'pool, 'f> JobHandle<'pool, 'f> {
    /// Block until the job is finished.
    ///
    /// # Panics
    ///
    /// This will panic if the job panicked.
    pub fn wait(mut self) {
        self.wait = false;
        self.pool.job_finished.recv().unwrap().unwrap();
    }
}
#[unsafe_destructor]
impl<'pool, 'f> Drop for JobHandle<'pool, 'f> {
    fn drop(&mut self) {
        if self.wait {
            self.pool.job_finished.recv().unwrap().unwrap();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.job_queue.send(None).unwrap();
        self.job_finished.recv().unwrap().unwrap();
    }
}
struct PanicHandler {
    tx: mpsc::Sender<Result<(), ()>>
}
impl Drop for PanicHandler {
    fn drop(&mut self) {
        let msg = if thread::panicking() { Err(()) } else { Ok(()) };
        self.tx.send(msg).unwrap();
    }
}
struct PanicCanary<'a> {
    flag: &'a atomic::AtomicBool
}
#[unsafe_destructor]
impl<'a> Drop for PanicCanary<'a> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.flag.store(true, atomic::Ordering::SeqCst)
        }
    }
}
impl Pool {
    /// Create a new thread pool with `n_threads` worker threads.
    pub fn new(n_threads: usize) -> Pool {
        let (tx, rx) = mpsc::channel::<Option<Job>>();
        let (finished_tx, finished_rx) = mpsc::channel();

        thread::spawn(move || {
            let ref panicked = atomic::AtomicBool::new(false);

            let mut _guards = Vec::with_capacity(n_threads);
            let mut txs = Vec::with_capacity(n_threads);
            let finished_tx = PanicHandler {
                tx: finished_tx,
            };

            for i in 0..n_threads {
                let id = WorkerId { n: i };
                let (subtx, subrx) = mpsc::channel::<Work>();
                txs.push(subtx);

                _guards.push(thread::scoped(move || {
                    let _canary = PanicCanary {
                        flag: panicked
                    };
                    loop {
                        match subrx.recv() {
                            Ok(mut work) => {
                                (work.func)(id)
                            }
                            Err(_) => break,
                        }
                    }
                }))
            }

            while let Ok(Some(job)) = rx.recv() {
                job.func.invoke(&txs);
                let job_panicked = panicked.load(atomic::Ordering::SeqCst);
                let msg = if job_panicked { Err(()) } else { Ok(()) };
                finished_tx.tx.send(msg).unwrap();

                if job_panicked { break }
            }
        });

        Pool {
            job_queue: tx,
            job_finished: finished_rx,
            n_threads: n_threads,
        }
    }

    /// Execute `f` on each element of `iter`.
    ///
    /// This panics if `f` panics, although the precise time and
    /// number of elements consumed after the element that panics is
    /// not specified.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use simple_parallel::Pool;
    ///
    /// let mut pool = Pool::new(4);
    ///
    /// let mut v = [0; 8];
    ///
    /// // set each element, in parallel
    /// pool.for_(&mut v, |element| *element = 3);
    ///
    /// assert_eq!(v, [3; 8]);
    /// ```
    pub fn for_<Iter: IntoIterator, F>(&mut self, iter: Iter, ref f: F)
        where Iter::Item: Send,
              Iter: Send,
              F: Fn(Iter::Item) + Sync

    {
        let (needwork_tx, needwork_rx) = mpsc::channel();
        let mut work_txs = Vec::with_capacity(self.n_threads);
        let mut work_rxs = Vec::with_capacity(self.n_threads);
        for _ in 0..self.n_threads {
            let (t, r) = mpsc::channel();
            work_txs.push(t);
            work_rxs.push(r);
        }

        let mut work_rxs = work_rxs.into_iter();

        unsafe {
            let handle = self.execute(
                needwork_tx,
                |needwork_tx| {
                    let mut needwork_tx = Some(needwork_tx.clone());
                    let mut work_rx = Some(work_rxs.next().unwrap());
                    move |id| {
                        let work_rx = work_rx.take().unwrap();
                        let needwork = needwork_tx.take().unwrap();
                        loop {
                            needwork.send(id).unwrap();
                            match work_rx.recv() {
                                Ok(Some(elem)) => {
                                    f(elem);
                                }
                                Ok(None) | Err(_) => break
                            }
                        }
                    }
                },
                move |needwork_tx| {
                    let mut iter = iter.into_iter().fuse();
                    drop(needwork_tx);
                    loop {
                        match needwork_rx.recv() {
                            // closed, done!
                            Err(_) => break,
                            Ok(id) => {
                                work_txs[id.n].send(iter.next()).unwrap();
                            }
                        }
                    }
                });

            handle.wait();
        }
    }

    /// Execute `f` on each element in `iter` in parallel across the
    /// pool's threads, with unspecified yield order.
    ///
    /// This behaves like `map`, but does not make efforts to ensure
    /// that the elements are returned in the order of `iter`, hence
    /// this is cheaper.
    ///
    /// The iterator yields `(uint, T)` tuples, where the `uint` is
    /// the index of the element in the original iterator.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use simple_parallel::Pool;
    ///
    /// let mut pool = Pool::new(4);
    ///
    /// // adjust each element in parallel, and iterate over them as
    /// // they are generated (or as close to that as possible)
    /// let f = |i| i + 10;
    /// for (index, output) in pool.unordered_map(0..8, &f) {
    ///     // each element is exactly 10 more than its original index
    ///     assert_eq!(output, index + 10);
    /// }
    /// ```
    pub fn unordered_map<'pool, 'a, I: IntoIterator, F, T>(&'pool mut self, iter: I, f: &'a F)
        -> UnorderedParMap<'pool, 'a, T>
        where I: 'a + Send,
              I::Item: Send + 'a,
              F: 'a + Sync + Fn(I::Item) -> T,
              T: Send + 'a
    {
        let (needwork_tx, needwork_rx) = mpsc::channel();
        let mut work_txs = Vec::with_capacity(self.n_threads);
        let mut work_rxs = Vec::with_capacity(self.n_threads);
        for _ in 0..self.n_threads {
            let (t, r) = mpsc::channel();
            work_txs.push(t);
            work_rxs.push(r);
        }

        let mut work_rxs = work_rxs.into_iter();

        let (tx, rx) = mpsc::channel();

        let handle = unsafe {
            self.execute(needwork_tx,
                         move |needwork_tx| {
                             let mut needwork_tx = Some(needwork_tx.clone());
                             let mut work_rx = Some(work_rxs.next().unwrap());
                             let tx = tx.clone();
                             move |id| {
                                 let work_rx = work_rx.take().unwrap();
                                 let needwork = needwork_tx.take().unwrap();
                                 loop {
                                     needwork.send(id).unwrap();
                                     match work_rx.recv() {
                                         Ok(Some((idx, elem))) => {
                                             let data = f(elem);
                                             let status = tx.send(Packet {
                                                 idx: idx, data: Some(data)
                                             });
                                             // the user disconnected,
                                             // so there's no point
                                             // computing more.
                                             if status.is_err() {
                                                 break
                                             }
                                         }
                                         Ok(None) | Err(_) => break
                                     }
                                 }
                             }
                         },
                         move |needwork_tx| {
                             let mut iter = iter.into_iter().fuse().enumerate();
                             drop(needwork_tx);
                             loop {
                                 match needwork_rx.recv() {
                                     // closed, done!
                                     Err(_) => break,
                                     Ok(id) => {
                                         work_txs[id.n].send(iter.next()).unwrap();
                                     }
                                 }
                             }
                         })
        };

        UnorderedParMap {
            rx: rx,
            _guard: handle,
        }
    }

    /// Execute `f` on `iter` in parallel across the pool's threads,
    /// returning an iterator that yields the results in the order of
    /// the elements of `iter` to which they correspond.
    ///
    /// This is a drop-in replacement for `iter.map(f)`, that runs in
    /// parallel, and consumes `iter` as the pool's threads complete
    /// their previous tasks.
    ///
    /// See `unordered_map` if the output order is unimportant.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use simple_parallel::Pool;
    ///
    /// let mut pool = Pool::new(4);
    ///
    /// // create a vector by adjusting 0..8, in parallel
    /// let f = |i| i + 10;
    /// let elements: Vec<_> = pool.map(0..8, &f).collect();
    ///
    /// assert_eq!(elements, &[10, 11, 12, 13, 14, 15, 16, 17]);
    /// ```
    pub fn map<'pool, 'a, I: IntoIterator, F, T>(&'pool mut self, iter: I, f: &'a F)
        -> ParMap<'pool, 'a, T>
        where I: 'a + Send,
              I::Item: Send + 'a,
              F: 'a + Sync + Fn(I::Item) -> T,
              T: Send + 'a
    {
        ParMap {
            unordered: self.unordered_map(iter, f),
            looking_for: 0,
            queue: BinaryHeap::new(),
        }
    }
}

/// Low-level/internal functionality.
impl Pool {
    /// Run a job on the thread pool.
    ///
    /// `gen_fn` is called `self.n_threads` times to create the
    /// functions to execute on the worker threads. Each of these is
    /// immediately called exactly once on a worker thread (that is,
    /// they are semantically `FnOnce`), and `main_fn` is also called,
    /// on the supervisor thread. It is expected that the workers and
    /// `main_fn` will manage any internal coordination required to
    /// distribute chunks of work.
    ///
    /// The job must take pains to ensure `main_fn` doesn't quit
    /// before the workers do.
    pub unsafe fn execute<'pool, 'f, A, GenFn, WorkerFn, MainFn>(
        &'pool mut self, data: A, gen_fn: GenFn, main_fn: MainFn) -> JobHandle<'pool, 'f>

        where A: 'f + Send,
              GenFn: 'f + FnMut(&mut A) -> WorkerFn + Send,
              WorkerFn: 'f + FnMut(WorkerId) + Send,
              MainFn: 'f + FnOnce(A) + Send,
    {
        self.execute_nonunsafe(data, gen_fn, main_fn)
    }

    // separate function to ensure we get `unsafe` checking inside this one
    fn execute_nonunsafe<'pool, 'f, A, GenFn, WorkerFn, MainFn>(
        &'pool mut self, mut data: A,
        mut gen_fn: GenFn, main_fn: MainFn) -> JobHandle<'pool, 'f>

        where A: 'f + Send,
              GenFn: 'f + FnMut(&mut A) -> WorkerFn + Send,
              WorkerFn: 'f + FnMut(WorkerId) + Send,
              MainFn: 'f + FnOnce(A) + Send,
    {
        let n_threads = self.n_threads;
        // transmutes scary? only a little: the returned `JobHandle`
        // ensures safety by connecting this job to the outside stack
        // frame.
        let func: JobInner<'f> = Box::new(move |workers: &[mpsc::Sender<Work>]| {
            assert_eq!(workers.len(), n_threads);
            let mut worker_fns: Vec<_> = (0..n_threads).map(|_| gen_fn(&mut data)).collect();

            for (func, worker) in worker_fns.iter_mut().zip(workers.iter()) {
                let func: WorkInner = func;
                let func: WorkInner<'static> = unsafe {
                    mem::transmute(func)
                };
                worker.send(Work { func: func }).unwrap();
            }

            main_fn(data)
        });
        let func: JobInner<'static> = unsafe {
            mem::transmute(func)
        };
        self.job_queue.send(Some(Job { func: func })).unwrap();

        JobHandle {
            pool: self,
            wait: true,
            _funcs: marker::PhantomData,
        }
    }
}


use std::cmp::Ordering;

struct Packet<T> {
    // this should be unique for a given instance of `*ParMap`
    idx: usize,
    data: Option<T>,
}
impl<T> PartialOrd for Packet<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl<T> Ord for Packet<T> {
    // reverse the ordering, to work with the max-heap
    fn cmp(&self, other: &Self) -> Ordering { other.idx.cmp(&self.idx) }
}
impl<T> PartialEq for Packet<T> {
    fn eq(&self, other: &Self) -> bool { self.idx == other.idx }
}
impl<T> Eq for Packet<T> {}

/// A parallel-mapping iterator, that yields elements in the order
/// they are computed, not the order from which they are yielded by
/// the underlying iterator.
pub struct UnorderedParMap<'pool, 'a, T: 'a + Send> {
    rx: mpsc::Receiver<Packet<T>>,
    _guard: JobHandle<'pool, 'a>,
}
impl<'pool, 'a,T: 'a + Send> Iterator for UnorderedParMap<'pool , 'a, T> {
    type Item = (usize, T);

    fn next(&mut self) -> Option<(usize, T)> {
        match self.rx.recv() {
            Ok(Packet { data: Some(x), idx }) => Some((idx, x)),
            Ok(Packet { data: None, .. }) => {
                panic!("simple_parallel::unordered_map: closure panicked")
            }
            Err(mpsc::RecvError) => None,
        }
    }
}

/// A parallel-mapping iterator, that yields elements in the order
/// they are yielded by the underlying iterator.
pub struct ParMap<'pool, 'a, T: 'a + Send> {
    unordered: UnorderedParMap<'pool, 'a, T>,
    looking_for: usize,
    queue: BinaryHeap<Packet<T>>
}

impl<'pool, 'a, T: Send + 'a> Iterator for ParMap<'pool, 'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        loop {
            if self.queue.peek().map_or(false, |x| x.idx == self.looking_for) {
                // we've found what we want, so lets return it

                let packet = self.queue.pop().unwrap();
                self.looking_for += 1;
                match packet.data {
                    Some(x) => return Some(x),
                    None => panic!("simple_parallel::map: closure panicked")
                }
            }
            match self.unordered.rx.recv() {
                // this could be optimised to check for `packet.idx ==
                // self.looking_for` to avoid the BinaryHeap
                // interaction if its what we want.
                Ok(packet) => self.queue.push(packet),
                // all done
                Err(mpsc::RecvError) => return None,
            }
        }
    }
}
