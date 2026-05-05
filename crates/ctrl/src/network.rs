use std::sync::{
    Arc,
    atomic::{AtomicU16, Ordering},
};

#[derive(Debug, Clone)]
pub struct PortAllocator {
    next: Arc<AtomicU16>,
}

impl Default for PortAllocator {
    fn default() -> Self {
        Self {
            next: Arc::new(AtomicU16::new(3100)),
        }
    }
}

impl PortAllocator {
    pub fn next(&self) -> u16 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}
