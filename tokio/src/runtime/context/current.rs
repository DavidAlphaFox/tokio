use super::{Context, CONTEXT};

use crate::runtime::{scheduler, TryCurrentError};
use crate::util::markers::SyncNotSend;

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;

#[derive(Debug)]
#[must_use]
pub(crate) struct SetCurrentGuard {
    // The previous handle
    prev: Option<scheduler::Handle>,

    // The depth for this guard
    depth: usize,

    // Don't let the type move across threads.
    _p: PhantomData<SyncNotSend>,
} //该数据不会在线程之间移动

pub(super) struct HandleCell {
    /// Current handle
    handle: RefCell<Option<scheduler::Handle>>,

    /// Tracks the number of nested calls to `try_set_current`.
    depth: Cell<usize>,
}

/// Sets this [`Handle`] as the current active [`Handle`].
///
/// [`Handle`]: crate::runtime::scheduler::Handle
pub(crate) fn try_set_current(handle: &scheduler::Handle) -> Option<SetCurrentGuard> {
    CONTEXT.try_with(|ctx| ctx.set_current(handle)).ok()
}

pub(crate) fn with_current<F, R>(f: F) -> Result<R, TryCurrentError>
where
    F: FnOnce(&scheduler::Handle) -> R,
{
    match CONTEXT.try_with(|ctx| ctx.current.handle.borrow().as_ref().map(f)) {
        Ok(Some(ret)) => Ok(ret),
        Ok(None) => Err(TryCurrentError::new_no_context()),
        Err(_access_error) => Err(TryCurrentError::new_thread_local_destroyed()),
    }
}

impl Context {
    pub(super) fn set_current(&self, handle: &scheduler::Handle) -> SetCurrentGuard {
        let old_handle = self.current.handle.borrow_mut().replace(handle.clone());//用现在的handle替老的handle
        let depth = self.current.depth.get(); //获得依赖层级

        assert!(depth != usize::MAX, "reached max `enter` depth");

        let depth = depth + 1; //每次更换handler都要增加一个层级
        self.current.depth.set(depth);

        SetCurrentGuard {
            prev: old_handle, //之前的handler
            depth, //深度
            _p: PhantomData,
        }
    }
}

impl HandleCell {
    pub(super) const fn new() -> HandleCell {
        HandleCell {
            handle: RefCell::new(None),
            depth: Cell::new(0),
        }
    }
}

impl Drop for SetCurrentGuard {
    fn drop(&mut self) {
        CONTEXT.with(|ctx| {
            let depth = ctx.current.depth.get(); //得到调度器当前深度

            if depth != self.depth { //深度不等，这将是个问题
                if !std::thread::panicking() {
                    panic!(
                        "`EnterGuard` values dropped out of order. Guards returned by \
                         `tokio::runtime::Handle::enter()` must be dropped in the reverse \
                         order as they were acquired."
                    );
                } else {
                    // Just return... this will leave handles in a wonky state though...
                    return;
                }
            }

            *ctx.current.handle.borrow_mut() = self.prev.take(); //还原到前一个handle
            ctx.current.depth.set(depth - 1);
        });
    }
}
