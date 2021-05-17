use std::{fmt::Debug, future::Future, marker::PhantomData, mem::ManuallyDrop, pin::Pin, task::{Context, Poll}};

use tokio::{runtime::Runtime, sync::mpsc, task::{JoinError, JoinHandle}};

/// Borrows a tokio Runtime to construct a `ScopeBuilder` which can be used
/// to create a scope.
///
/// # Example
/// ```
/// # extern crate tokio_scope;
/// # extern crate futures;
/// # use futures::future::lazy;
/// 
/// let mut v = String::from("Hello, World")   ;
/// let rt = tokio::runtime::Runtime::new().unwrap();
/// tokio_scope::scoped(&rt).scope(|scope| {
/// // Use the scope to spawn the future.
/// scope.spawn(lazy(|_| {
///     v.push('!');
///     }));
/// });
/// assert_eq!(v.as_str(), "Hello, World!");
/// ```
pub fn scoped(rt: &Runtime) -> ScopeBuilder<'_> {
    ScopeBuilder::from_runtime(rt)
}

/// Wrapper type around a tokio Rentime which can be used to create `Scope`s.
/// This type takes ownership of the Runtime.
#[derive(Debug)]
pub struct ScopedRuntime(Runtime);

impl ScopedRuntime {
    pub fn new(rt: Runtime) -> Self {
        ScopedRuntime(rt)
    }

    /// Creates a scope bound by the lifetime of `self` that can be used to spawn scoped futures.
    pub fn scope<'a, F, R>(&'a self, f: F) -> R
    where
    F: FnOnce(&Scope<'a>) -> R,
    {
        let scope = Scope::new(&self.0);
        f(&scope)
    }

    /// Consumes the `ScopedRuntime` and returns the inner [`Runtime`] variable.
    ///
    /// [`Runtime`]: https://docs.rs/tokio/1.6.0/runtime/struct.Runtime.html
    pub fn into_inner(self) -> Runtime {
        self.0
    }
}

/// Struct used to build scopes from a borrowed Runitem. Generally users should
/// use the [`scoped`] function.
///
/// [`scoped`]: /tokio-scope/fn.scoped.html
#[derive(Debug)]
pub struct ScopeBuilder<'a>(&'a Runtime);


impl<'a> ScopeBuilder<'a> {
    pub fn from_runtime(rt: &'a Runtime) -> Self {
        Self(rt)
    }

    pub fn scope<F, R>(&self, f: F) -> R
    where
    F: FnOnce(&Scope<'a>) -> R,
    {
        let scope = Scope::new(self.0);
        f(&scope)
    }
}

#[derive(Debug)]
pub struct Scope<'a> {
    rt: &'a Runtime,
    send: ManuallyDrop<mpsc::Sender<()>>,
    // When the `Scope` is dropped, we wait on this receiver to close.
    // No message are sent through the receiver, however, the `Sender`
    // objects get cloned into each spawned future (see `ScopedFuture`).
    // This is how we ensure they all exit eventually.
    recv: Option<mpsc::Receiver<()>>,
    _marker: PhantomData<&'a ()>,
}

impl<'a> Scope<'a> {
    fn new(rt: &'a Runtime) -> Self {
        let (send, recv) = mpsc::channel(1);
        Self {
            rt,
            send: ManuallyDrop::new(send),
            recv: Some(recv),
            _marker: PhantomData
        }
    }

    fn scoped_future<'s, F, R>(&'s self, f: F) -> ScopedFuture<R>
    where
    F: Future<Output = R> + Send + 'a,
    R: Send + 'a,
    'a: 's,
    {
        let boxed: Pin<Box<dyn Future<Output = R> + Send + 'a>> = Box::pin(f);
        // This transmute sohuld be safe, as we use the `ScopedFuture` abstraction to
        // prevent the scope form exiting unitl every spowned `ScopedFuture` object is
        // dropped, signifying that they have completed their execution.
        let boxed: Pin<Box<dyn Future<Output = R> + Send + 'static>> = unsafe {
            std::mem::transmute(boxed)
        };
        ScopedFuture {
            f: boxed,
            _marker: (*self.send).clone()
        }
    }

    /// Spawn the received future on the [`ScopedRuntime`] which was used te create `self`.
    ///
    /// [`ScopedRuntime`]: /tokio-scope/struct.ScopedRuntime.html
    pub fn spawn<'s, F, R>(&'s self, future: F) -> ScopeHandle<R>
    where
    F: Future<Output = R> + Send + 'a,
    R: Send + 'static,
    'a: 's,
    {
        let scoped_f = self.scoped_future(future);
        let handle = Box::pin(self.rt.spawn(scoped_f));
        let scoped_handle = ScopeHandle {
            handle,
            _marker: (*self.send).clone()
        };
        //self
        scoped_handle
    }

    /*
    /// Blocks the "current thread" of the runtime until `future` resolves. Other spawned
    /// futures can make progress while this future is running.
    pub fn block_on<'s, R, F>(&'s self, future: F) -> R
    where
    F: Future<Output = R> + Send + 'a,
    R: Debug + Send + 'a,
    'a: 's,
    {
        let (tx, rx) = oneshot::channel();
        let future = future.then(|r| async move {
            tx.send(r).unwrap();
        });
        let boxed: Pin<Box<dyn Future<Output = ()> + Send + 'a>> = Box::pin(future);
        let boxed:Pin<Box<dyn Future<Output = ()> + Send + 'static>> = unsafe {
            std::mem::transmute(boxed)
        };
        self.rt.spawn(boxed);
        let rt  = Runtime::new().unwrap();
        rt.block_on(rx).unwrap()
    }
    */

    /// Creates an `inner` scope which can access variables created within the outer scope.
    pub fn scope<'inner, F, R>(&'inner self, f: F) -> R
    where
    F: FnOnce(&Scope<'inner>) -> R,
    'a: 'inner,
    {
        let scope = Scope::new(self.rt);
        f(&scope)
    }

    /// Get a reference to the underlying `Runtime` instance.
    pub fn runtime(&self) -> &Runtime {
        self.rt
    }
}

impl<'a> Drop for Scope<'a> {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.send);
        }
        let mut recv = self.recv.take().unwrap();
        let n = recv.blocking_recv();
        assert_eq!(n, None);
    }
}

pub struct ScopedFuture<R> {
    f: Pin<Box<dyn Future<Output = R> + Send + 'static>>,
    _marker: mpsc::Sender<()>,
}

impl<R> Future for ScopedFuture<R> {
    type Output = R;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.f.as_mut().poll(cx)
    }
}

pub struct ScopeHandle<R> {
    handle: Pin<Box<JoinHandle<R>>>,
    _marker: mpsc::Sender<()>,
}

impl<R> Future for ScopeHandle<R> {
    type Output = Result<R, JoinError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.handle.as_mut().poll(cx)
    }
}

impl<R> ScopeHandle<R> {
    pub fn abort(&self) {
        self.handle.abort()
    }
}

#[cfg(test)]
mod testing {
    use super::*;
    use futures::future::lazy;
    use std::{thread, time::Duration};
    use tokio;
    
    fn make_runtime() -> ScopedRuntime {
        let rt = Runtime::new().expect("Failed to construct Runtime");
        ScopedRuntime::new(rt)
    }

    #[test]
    fn basic_test() {
        let scoped = make_runtime();
        scoped.scope(|scope| {
            scope.spawn(lazy(|_| {
                tokio::spawn(lazy(|_| {
                    println!("Another!");
                    thread::sleep(Duration::from_millis(500));
                    println!("Another is done sleeping");
                }));

                println!("Sleeping a spawned future");
                thread::sleep(Duration::from_millis(200));
                println!("Completing!");
            }));
        });
        println!("Completed");
    }

    #[test]
    fn access_stack() {
        let scoped = make_runtime();
        // Specifically a variable that does _not_ implement `Copy`.
        let uncopy = String::from("Borrowed!");
        scoped.scope(|scope| {
            scope.spawn(lazy(|_| {
                assert_eq!(uncopy.as_str(), "Borrowed!");
                println!("Borrowed sucessfully: {}", uncopy);
            }));
        });
    }
/*
    #[test]
    fn access_mut_stack() {
        let scoped = make_runtime();
        let mut uncopy = String::from("Borrowed");
        let mut uncopy2 = String::from("Borrowed");
        scoped.scope(|scope| {
            scope.spawn(lazy(|_| {
                println!("a");
                let f = scoped
                    .scope(|scope2| scope2.block_on(lazy(|_| 4)));
                println!("b");
                assert_eq!(f, 4);
                thread::sleep(Duration::from_millis(1000));
                uncopy.push('!');
            }));

            scope.spawn(lazy(|_| {
                uncopy2.push('f');
            }));
        });

        assert_eq!(uncopy.as_str(), "Borrowed!");
        assert_eq!(uncopy2.as_str(), "Borrowedf");
    }

    #[test]
    fn block_on_test() {
        let scoped = make_runtime();
        let mut uncopy = String::from("Borrowed");
        let captured = scoped.scope(|scope| {
            let v = scope
                .block_on(lazy(|_| {
                    uncopy.push('!');
                    Ok::<_, ()>(uncopy)
                }))
                .unwrap();
            assert_eq!(v.as_str(), "Borrowed!");
            v
        });
        assert_eq!(captured.as_str(), "Borrowed!");
    }
*/
    #[test]
    fn borrow_many_test() {
        let scoped = make_runtime();
        let mut values = vec![1, 2, 3, 4];
        scoped.scope(|scope| {
            for v in &mut values {
                scope.spawn(lazy(move |_| {
                    *v += 1;
                    
                }));
            }
        });

        assert_eq!(&values, &[2, 3, 4, 5]);
    }

    #[test]
    fn inner_scope_test() {
        let scoped = make_runtime();
        let mut values = vec![1, 2, 3, 4];
        scoped.scope(|scope| {
            let mut v2s = vec![2, 3, 4, 5];
            scope.scope(|scope2| {
                scope2.spawn(lazy(|_| {
                    v2s.push(100);
                    values.push(100);
                }));
            });
            // The inner scope must exit before we can get here.
            assert_eq!(v2s, &[2, 3, 4, 5, 100]);
            assert_eq!(values, &[1, 2, 3, 4, 100]);
        });
    }

    #[test]
    fn borrowed_scope_test() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut values = vec![1, 2, 3, 4];
        scoped(&rt).scope(|scope| {
            scope.spawn(lazy(|_| {
                values.push(100);
            }));
        });
        assert_eq!(values, &[1, 2, 3, 4, 100]);
    }

    #[test]
    fn scope_handle_test() {
        let scoped = make_runtime();
        let mut v = String::from("Hello");
        scoped.scope(|scope| {
            let h = scope.spawn(async {
                v.push('!');
            });
            scope.spawn(async {
                h.await;
                v.push('?');
            });
        });
        assert_eq!(v.as_str(), "Hello!?");
    }
}