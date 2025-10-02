use std::sync::Arc;
use parking_lot::Mutex;

#[derive(Clone)]
pub struct CancelToken {
    inner: Arc<Mutex<CancelTokenInner>>,
}
struct CancelTokenInner {
    cancelled: bool,
    listeners: Vec<Box<dyn FnMut() + Send + Sync>>,
}
impl CancelToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CancelTokenInner {
                cancelled: false,
                listeners: Vec::new(),
            })),
        }
    }

    pub fn register_callback<F>(&self, callback: F)
    where
        F: FnMut() + Send + Sync + 'static,
    {
        let mut inner = self.inner.lock();
        inner.listeners.push(Box::new(callback));
    }

    pub fn cancelled(&self) -> bool {
        self.inner.lock().cancelled
    }

    pub fn cancel(&self) {
        let mut inner = self.inner.lock();
        if inner.cancelled {
            return;
        }
        inner.cancelled = true;
        for listener in &mut inner.listeners {
            listener();
        }
    }
}
