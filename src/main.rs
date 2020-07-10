use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread;
use std::time::Duration;

fn main() {
    let (tx_futures, rx_futures) = mpsc::channel();
    // 初始化Executor
    let executor = Executor::new(rx_futures, tx_futures.clone());
    thread::spawn(move || {
        executor.run();
    });
    // 初始化Runtime
    unsafe {
        let rt_ptr: *mut Runtime = &RT as *const _ as *mut _;
        (*rt_ptr).init(tx_futures);
    }
    // let r = RT.block_on(async { 2 });
    // let r = RT.block_on(FakeIo::new(Duration::from_secs(2)));
    let r = RT.block_on(async {
        let a = RT.spawn(FakeIo::new(Duration::from_secs(2))).await;
        a + 1
    });
    dbg!(r);
}

static RT: Runtime = Runtime::uninit();

struct FakeIo {
    val: Arc<Mutex<Option<i32>>>,
    start: bool,
    dur: Duration,
}

impl FakeIo {
    fn new(dur: Duration) -> Self {
        Self {
            val: Arc::new(Mutex::new(None)),
            start: false,
            dur,
        }
    }
}

impl Future for FakeIo {
    type Output = i32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let v = self.val.lock().unwrap();
        if v.is_some() {
            return Poll::Ready(v.unwrap());
        }
        drop(v);
        if !self.start {
            let w = cx.waker().clone();
            let v = self.val.clone();
            let dur = self.dur;
            unsafe {
                self.get_unchecked_mut().start = true;
            }
            thread::spawn(move || {
                thread::sleep(dur);
                *v.lock().unwrap() = Some(100);
                w.wake();
            });
        }
        Poll::Pending
    }
}

struct WakerFn<F>(F);

impl<F: Fn() + 'static + Send> WakerFn<F> {
    const VTABLE: RawWakerVTable =
        RawWakerVTable::new(Self::clone, Self::wake, Self::wake_by_ref, Self::drop);

    unsafe fn clone(data: *const ()) -> RawWaker {
        let arc = std::mem::ManuallyDrop::new(Arc::from_raw(data as *const F));
        // 增加引用计数，但是不执行析构方法，因为下面需要转换成裸指针，防止悬垂指针
        let _ = arc.clone();
        RawWaker::new(data, &Self::VTABLE)
    }

    unsafe fn wake(data: *const ()) {
        let arc = Arc::from_raw(data as *const F);
        arc();
    }

    unsafe fn wake_by_ref(data: *const ()) {
        let f = &*(data as *const F);
        f();
    }

    unsafe fn drop(data: *const ()) {
        drop(Arc::from_raw(data as *const F))
    }

    fn get_waker(f: F) -> Waker {
        let data = Arc::into_raw(Arc::new(f)) as *const ();
        let vtable = &Self::VTABLE;
        unsafe { Waker::from_raw(RawWaker::new(data, vtable)) }
    }
}

struct RawTask {
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

type Task = Arc<RawTask>;

struct Executor {
    rx_futures: Receiver<Task>,
    tx_futures: Sender<Task>,
}

impl Executor {
    fn new(rx_futures: Receiver<Task>, tx_futures: Sender<Task>) -> Self {
        Self {
            rx_futures,
            tx_futures,
        }
    }

    fn run(&self) {
        while let Ok(task) = self.rx_futures.recv() {
            let tx_futures_w = self.tx_futures.clone();
            let task_w = task.clone();
            let waker = WakerFn::get_waker(move || {
                // 重新调度任务
                tx_futures_w.send(task_w.clone()).unwrap();
            });
            let mut cx = Context::from_waker(&waker);
            let _ = task.future.lock().unwrap().as_mut().poll(&mut cx);
        }
    }
}

struct Runtime {
    tx_futures: Option<Sender<Task>>,
    // 由于 Sender 没有实现 Sync，所以实现一个简单的 Spin
    lock: AtomicBool,
}

unsafe impl Sync for Runtime {}

impl Runtime {
    const fn uninit() -> Self {
        Self {
            tx_futures: None,
            lock: AtomicBool::new(false),
        }
    }

    fn init(&mut self, tx_futures: Sender<Task>) {
        self.tx_futures = Some(tx_futures);
    }

    fn spawn<F, R>(&self, f: F) -> impl Future<Output = R>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let waiter = Arc::new(Waiter::<R>::new());
        let waiter_f = WaiterFuture::new(waiter.clone());
        // 包装task
        let f2 = async move {
            let r = f.await;
            waiter.complete(r);
        };
        let raw_task = RawTask {
            future: Mutex::new(Box::pin(f2)),
        };
        let task: Task = Arc::new(raw_task);

        // 获取锁
        while self.lock.compare_and_swap(false, true, Ordering::SeqCst) {}
        // 调度任务
        self.tx_futures
            .as_ref()
            .expect("Runtime未初始化")
            .send(task)
            .unwrap();
        // 释放锁
        self.lock.store(false, Ordering::SeqCst);
        waiter_f
    }

    fn block_on<F, R>(&self, f: F) -> F::Output
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        // 包装task
        let f2 = async move {
            let r = f.await;
            tx.send(r).unwrap();
        };
        let raw_task = RawTask {
            future: Mutex::new(Box::pin(f2)),
        };
        let task: Task = Arc::new(raw_task);
        // 获取锁
        while self.lock.compare_and_swap(false, true, Ordering::SeqCst) {}
        // 调度任务
        self.tx_futures
            .as_ref()
            .expect("Runtime未初始化")
            .send(task)
            .unwrap();
        // 释放锁
        self.lock.store(false, Ordering::SeqCst);
        rx.recv().unwrap()
    }
}

struct Waiter<T> {
    val: Mutex<Option<T>>,
    waker: Mutex<Option<Waker>>,
}

impl<T> Waiter<T> {
    fn new() -> Self {
        Self {
            val: Mutex::new(None),
            waker: Mutex::new(None),
        }
    }

    fn complete(&self, val: T) {
        *self.val.lock().unwrap() = Some(val);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        } else {
            // TODO:
        }
    }
}

struct WaiterFuture<T> {
    waiter: Arc<Waiter<T>>,
}

impl<T> WaiterFuture<T> {
    fn new(waiter: Arc<Waiter<T>>) -> Self {
        Self { waiter }
    }
}

impl<T> Future for WaiterFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let w = self.get_unchecked_mut();
            let mut val = w.waiter.val.lock().unwrap();
            if val.is_some() {
                Poll::Ready(val.take().unwrap())
            } else {
                // 注册waker
                let mut w_waker = w.waiter.waker.lock().unwrap();
                if w_waker.is_none() {
                    let waker = cx.waker().clone();
                    *w_waker = Some(waker);
                }
                Poll::Pending
            }
        }
    }
}
