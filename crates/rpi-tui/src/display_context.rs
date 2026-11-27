//! Palette and image capabilities for a terminal in a shared process.

use std::cell::RefCell;
use std::future::{poll_fn, Future};
use std::pin::pin;
use std::sync::{Arc, Mutex};

use crate::terminal_image::TerminalCapabilities;
use crate::theme::ThemeManager;

pub struct DisplayContext {
    pub(crate) theme: ThemeManager,
    pub(crate) images: Mutex<TerminalCapabilities>,
}

impl DisplayContext {
    pub fn new(images: TerminalCapabilities) -> Self {
        Self {
            theme: ThemeManager::new(),
            images: Mutex::new(images),
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<DisplayContext>>> = const { RefCell::new(None) };
}

pub fn current() -> Option<Arc<DisplayContext>> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Restores the previous context on the same thread when it goes out of scope.
pub struct DisplayScope {
    previous: Option<Option<Arc<DisplayContext>>>,
    thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for DisplayScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            CURRENT.with(|slot| slot.replace(previous));
        }
    }
}

pub fn enter(context: Option<&Arc<DisplayContext>>) -> DisplayScope {
    let previous = context.map(|context| CURRENT.with(|slot| slot.replace(Some(context.clone()))));
    DisplayScope {
        previous,
        thread: std::marker::PhantomData,
    }
}

pub fn with<R>(context: Option<&Arc<DisplayContext>>, f: impl FnOnce() -> R) -> R {
    let _scope = enter(context);
    f()
}

/// Re-enter the terminal's context on each poll, even if the task migrates.
pub async fn scope<F: Future>(context: Option<Arc<DisplayContext>>, future: F) -> F::Output {
    let mut future = pin!(future);
    poll_fn(|cx| with(context.as_ref(), || future.as_mut().poll(cx))).await
}
