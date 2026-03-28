use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub page_size: u32,
    pub total: u64,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, page: u32, page_size: u32, total: u64) -> Self {
        Self {
            items,
            page,
            page_size,
            total,
        }
    }
}
