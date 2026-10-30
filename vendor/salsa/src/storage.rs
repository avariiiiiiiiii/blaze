//! Public API facades for the implementation details of [`Zalsa`] and [`ZalsaLocal`].
use std::marker::PhantomData;

use crate::Database;
use crate::task::TaskLocals;
use crate::zalsa::{ErasedJar, HasJar, Zalsa, ZalsaDatabase};
use crate::zalsa_local::ZalsaLocal;

/// Access the "storage" of a Salsa database: this is an internal plumbing trait
/// automatically implemented by `#[salsa::db]` applied to a struct.
///
/// # Safety
///
/// The `storage` and `storage_mut` fields must both return a reference to the same
/// storage field which must be owned by `self`.
pub unsafe trait HasStorage: Database + Sized {
    fn storage(&self) -> &Storage<Self>;
    fn storage_mut(&mut self) -> &mut Storage<Self>;
}

/// The state of a Salsa database. It is `Sync`: one database is read through `&` from any
/// number of threads at once, each task with its own query stack (see [`set_task_depth`]), and
/// written through `&mut` alone. There are no second handles to a database.
///
/// [`set_task_depth`]: crate::set_task_depth
pub struct Storage<Db> {
    zalsa_impl: Box<Zalsa>,

    /// The query state of every task that reads this database.
    task_locals: TaskLocals,

    /// We store references to `Db`
    phantom: PhantomData<fn() -> Db>,
}

impl<Db: Database> Default for Storage<Db> {
    fn default() -> Self {
        Self::new(None)
    }
}

impl<Db: Database> Storage<Db> {
    /// Create a new database storage.
    ///
    /// The `event_callback` function is invoked by the salsa runtime at various points during execution.
    pub fn new(event_callback: Option<Box<dyn Fn(crate::Event) + Send + Sync + 'static>>) -> Self {
        Self::with_jars(event_callback, Vec::new())
    }

    fn with_jars(
        event_callback: Option<Box<dyn Fn(crate::Event) + Send + Sync + 'static>>,
        jars: Vec<ErasedJar>,
    ) -> Self {
        Self {
            zalsa_impl: Box::new(Zalsa::new::<Db>(event_callback, jars)),
            task_locals: TaskLocals::new(),
            phantom: PhantomData,
        }
    }

    /// Returns a builder for database storage.
    pub fn builder() -> StorageBuilder<Db> {
        StorageBuilder::default()
    }

    /// Prepares a write. `&mut` excludes every reader: a query on any thread holds `&Db`, so none
    /// runs now and there is nothing to cancel or wait for. What remains is to start a new
    /// cancellation epoch, so provisional memos a locally cancelled query left behind are told
    /// apart from those made after the write.
    fn prepare_write(&mut self) -> &mut Zalsa {
        let zalsa = &mut *self.zalsa_impl;
        self.task_locals.return_unfilled_pages(zalsa.table());
        let overflow = zalsa.runtime_mut().bump_cancellation_count();
        if overflow {
            zalsa.new_revision();
        }
        zalsa
    }
}

/// A builder for a [`Storage`] instance.
///
/// This type can be created with the [`Storage::builder`] function.
pub struct StorageBuilder<Db> {
    jars: Vec<ErasedJar>,
    event_callback: Option<Box<dyn Fn(crate::Event) + Send + Sync + 'static>>,
    _db: PhantomData<Db>,
}

impl<Db> Default for StorageBuilder<Db> {
    fn default() -> Self {
        Self {
            jars: Vec::new(),
            event_callback: None,
            _db: PhantomData,
        }
    }
}

impl<Db: Database> StorageBuilder<Db> {
    /// Set a callback for salsa events.
    ///
    /// The `event_callback` function will be invoked by the salsa runtime at various points during execution.
    pub fn event_callback(
        mut self,
        callback: Box<dyn Fn(crate::Event) + Send + Sync + 'static>,
    ) -> Self {
        self.event_callback = Some(callback);
        self
    }

    /// Manually register an ingredient.
    ///
    /// Manual ingredient registration is necessary when the `inventory` feature is disabled.
    pub fn ingredient<I: HasJar>(mut self) -> Self {
        self.jars.push(ErasedJar::erase::<I>());
        self
    }

    /// Construct the [`Storage`] using the provided builder options.
    pub fn build(self) -> Storage<Db> {
        Storage::with_jars(self.event_callback, self.jars)
    }
}

// SAFETY: `zalsa` and `zalsa_mut` return the storage's own `Zalsa`, and `zalsa_local` the state
// of the task the calling thread runs now, which lives while the storage does (`TaskLocals`).
unsafe impl<T: HasStorage> ZalsaDatabase for T {
    #[inline(always)]
    fn zalsa(&self) -> &Zalsa {
        &self.storage().zalsa_impl
    }

    fn zalsa_mut(&mut self) -> &mut Zalsa {
        self.storage_mut().prepare_write()
    }

    #[inline(always)]
    fn zalsa_local(&self) -> &ZalsaLocal {
        self.storage().task_locals.current()
    }
}
