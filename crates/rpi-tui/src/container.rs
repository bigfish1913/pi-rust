//! Container component that holds multiple child components.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;

/// Container - a component that contains other components.
///
/// Children are rendered in order, vertically stacked.
#[derive(Clone)]
pub struct Container {
    children: Arc<Mutex<Vec<Arc<dyn Component>>>>,
}

impl Container {
    /// Create a new empty container.
    pub fn new() -> Self {
        Self {
            children: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Add a child component.
    pub fn add_child(&self, component: Arc<dyn Component>) {
        if let Ok(mut children) = self.children.lock() {
            children.push(component);
        }
    }

    /// Remove a child component.
    pub fn remove_child(&self, component: &Arc<dyn Component>) {
        if let Ok(mut children) = self.children.lock() {
            children.retain(|c| !Arc::ptr_eq(c, component));
        }
    }

    /// Clear all children.
    pub fn clear(&self) {
        if let Ok(mut children) = self.children.lock() {
            children.clear();
        }
    }

    /// Get the number of children.
    pub fn child_count(&self) -> usize {
        self.children.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// Get a clone of the children list.
    pub fn get_children(&self) -> Vec<Arc<dyn Component>> {
        self.children.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();

        if let Ok(children) = self.children.lock() {
            for child in children.iter() {
                let child_lines = child.render(width);
                lines.extend(child_lines);
            }
        }

        lines
    }

    fn invalidate(&self) {
        if let Ok(children) = self.children.lock() {
            for child in children.iter() {
                child.invalidate();
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
