//! A library providing a generic connection pool.
#![feature(unsafe_destructor)]
#![warn(missing_docs)]
#![allow(unstable)]
#![doc(html_root_url="https://sfackler.github.io/doc")]

#[macro_use]
extern crate log;
extern crate time;

use std::collections::RingBuf;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, Condvar};
use std::fmt;

pub use config::{Config, ConfigError};

use task::ScheduledThreadPool;

mod config;
mod task;

/// A trait which provides database-specific functionality.
pub trait PoolManager<C, E>: Send+Sync {
    /// Attempts to create a new connection.
    fn connect(&self) -> Result<C, E>;

    /// Determines if the connection is still connected to the database.
    ///
    /// A standard implementation would check if a simple query like `SELECT 1`
    /// succeeds.
    fn is_valid(&self, conn: &mut C) -> Result<(), E>;

    /// *Quickly* determines if the connection is no longer usable.
    ///
    /// This will be called synchronously every time a connection is returned
    /// to the pool, so it should *not* block. If it returns `true`, the
    /// connection will be discarded.
    ///
    /// For example, an implementation might check if the underlying TCP socket
    /// has disconnected. Implementations that do not support this kind of
    /// fast health check may simply return `false`.
    fn has_broken(&self, conn: &mut C) -> bool;
}

/// A trait which handles errors reported by the `PoolManager`.
pub trait ErrorHandler<E>: Send+Sync {
    /// Handles an error.
    fn handle_error(&self, error: E);
}

impl<E> ErrorHandler<E> for Box<ErrorHandler<E>> {
    fn handle_error(&self, error: E) {
        (**self).handle_error(error)
    }
}

/// An `ErrorHandler` which does nothing.
#[derive(Copy, Clone, Debug)]
pub struct NoopErrorHandler;

impl<E> ErrorHandler<E> for NoopErrorHandler {
    fn handle_error(&self, _: E) {}
}

/// An `ErrorHandler` which logs at the error level.
#[derive(Copy, Clone, Debug)]
pub struct LoggingErrorHandler;

impl<E> ErrorHandler<E> for LoggingErrorHandler where E: fmt::Debug {
    fn handle_error(&self, error: E) {
        error!("{:?}", error);
    }
}

struct PoolInternals<C> {
    conns: RingBuf<C>,
    num_conns: u32,
    thread_pool: ScheduledThreadPool,
}

unsafe impl<C: Send> Send for PoolInternals<C> {}

struct SharedPool<C, E, M, H> where C: Send, E: Send, M: PoolManager<C, E>, H: ErrorHandler<E> {
    config: Config,
    manager: M,
    error_handler: H,
    internals: Mutex<PoolInternals<C>>,
    cond: Condvar,
}

fn add_connection<C, E, M, H>(shared: &Arc<SharedPool<C, E, M, H>>)
        where C: Send, E: Send, M: PoolManager<C, E>, H: ErrorHandler<E> {
    let new_shared = shared.clone();
    shared.internals.lock().unwrap().thread_pool.run(move || {
        let shared = new_shared;
        match shared.manager.connect() {
            Ok(conn) => {
                let mut internals = shared.internals.lock().unwrap();
                internals.conns.push_back(conn);
                internals.num_conns += 1;
                shared.cond.notify_one();
            }
            Err(err) => shared.error_handler.handle_error(err),
        }
    });
}

/// A generic connection pool.
pub struct Pool<C, E, M, H> where C: Send, E: Send, M: PoolManager<C, E>, H: ErrorHandler<E> {
    shared: Arc<SharedPool<C, E, M, H>>,
}

impl<C, E, M, H> fmt::Debug for Pool<C, E, M, H>
        where C: Send, E: Send, M: PoolManager<C, E>+fmt::Debug, H: ErrorHandler<E> {
    // FIXME there's more we can do here
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Pool {{ config: {:?}, manager: {:?} }}", self.shared.config,
               self.shared.manager)
    }
}

impl<C, E, M, H> Pool<C, E, M, H>
        where C: Send, E: Send, M: PoolManager<C, E>, H: ErrorHandler<E> {
    /// Creates a new connection pool.
    ///
    /// Returns an `Err` value only if `config` is invalid.
    pub fn new(config: Config, manager: M, error_handler: H)
               -> Result<Pool<C, E, M, H>, ConfigError> {
        try!(config.validate());

        let internals = PoolInternals {
            conns: RingBuf::new(),
            num_conns: config.pool_size,
            thread_pool: ScheduledThreadPool::new(config.helper_tasks as usize),
        };

        let shared = Arc::new(SharedPool {
            config: config,
            manager: manager,
            error_handler: error_handler,
            internals: Mutex::new(internals),
            cond: Condvar::new(),
        });

        for _ in range(0, config.pool_size) {
            add_connection(&shared);
        }

        Ok(Pool {
            shared: shared,
        })
    }

    /// Retrieves a connection from the pool.
    pub fn get<'a>(&'a self) -> Result<PooledConnection<'a, C, E, M, H>, ()> {
        let mut internals = self.shared.internals.lock().unwrap();

        loop {
            match internals.conns.pop_front() {
                Some(mut conn) => {
                    drop(internals);

                    if self.shared.config.test_on_check_out {
                        if let Err(e) = self.shared.manager.is_valid(&mut conn) {
                            self.shared.error_handler.handle_error(e);
                            internals = self.shared.internals.lock().unwrap();
                            internals.num_conns -= 1;
                            continue
                        }
                    }

                    return Ok(PooledConnection {
                        pool: self,
                        conn: Some(conn),
                    })
                }
                None => {
                    internals = self.shared.cond.wait(internals).unwrap();
                }
            }
        }
    }

    fn put_back(&self, mut conn: C) {
        // This is specified to be fast, but call it before locking anyways
        let broken = self.shared.manager.has_broken(&mut conn);

        let mut internals = self.shared.internals.lock().unwrap();
        if broken {
            internals.num_conns -= 1;
        } else {
            internals.conns.push_back(conn);
            self.shared.cond.notify_one();
        }
    }
}

/// A smart pointer wrapping a connection.
pub struct PooledConnection<'a, C, E, M, H>
        where C: Send, E: Send, M: PoolManager<C, E>, H: ErrorHandler<E> {
    pool: &'a Pool<C, E, M, H>,
    conn: Option<C>,
}

impl<'a, C, E, M, H> fmt::Debug for PooledConnection<'a, C, E, M, H>
        where C: Send+fmt::Debug, E: Send, M: PoolManager<C, E>+fmt::Debug, H: ErrorHandler<E> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "PooledConnection {{ pool: {:?}, connection: {:?} }}", self.pool,
               self.conn.as_ref().unwrap())
    }
}

#[unsafe_destructor]
impl<'a, C, E, M, H> Drop for PooledConnection<'a, C, E, M, H>
        where C: Send, E: Send, M: PoolManager<C, E>, H: ErrorHandler<E> {
    fn drop(&mut self) {
        self.pool.put_back(self.conn.take().unwrap());
    }
}

impl<'a, C, E, M, H> Deref for PooledConnection<'a, C, E, M, H>
        where C: Send, E: Send, M: PoolManager<C, E>, H: ErrorHandler<E> {
    type Target = C;

    fn deref(&self) -> &C {
        self.conn.as_ref().unwrap()
    }
}

impl<'a, C, E, M, H> DerefMut for PooledConnection<'a, C, E, M, H>
        where C: Send, E: Send, M: PoolManager<C, E>, H: ErrorHandler<E> {
    fn deref_mut(&mut self) -> &mut C {
        self.conn.as_mut().unwrap()
    }
}
