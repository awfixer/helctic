//! Context management

use core::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT, Ordering};
use spin::{Once, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub use self::context::Context;
pub use self::list::ContextList;
pub use self::switch::switch;

/// Context struct
mod context;

/// Context list
mod list;

/// Context switch function
mod switch;

/// File struct - defines a scheme and a file number
pub mod file;

/// Memory struct - contains a set of pages for a context
pub mod memory;

/// Limit on number of contexts
pub const CONTEXT_MAX_CONTEXTS: usize = 65536;

/// Maximum context files
pub const CONTEXT_MAX_FILES: usize = 65536;

/// Contexts list
static CONTEXTS: Once<RwLock<ContextList>> = Once::new();

#[thread_local]
static CONTEXT_ID: AtomicUsize = ATOMIC_USIZE_INIT;

pub fn init() {
    let mut contexts = contexts_mut();
    let context_lock = contexts.new_context().expect("could not initialize first context");
    let mut context = context_lock.write();
    context.running = true;
    context.blocked = false;
    CONTEXT_ID.store(context.id, Ordering::SeqCst);
}

/// Initialize contexts, called if needed
fn init_contexts() -> RwLock<ContextList> {
    RwLock::new(ContextList::new())
}

/// Get the global schemes list, const
pub fn contexts() -> RwLockReadGuard<'static, ContextList> {
    CONTEXTS.call_once(init_contexts).read()
}

/// Get the global schemes list, mutable
pub fn contexts_mut() -> RwLockWriteGuard<'static, ContextList> {
    CONTEXTS.call_once(init_contexts).write()
}
