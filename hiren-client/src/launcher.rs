//! Launcher state — the single source of truth exposed to the UI runtime.
//!
//! The Hiren UI runtime receives a LauncherState snapshot and reacts automatically.
//! The launcher logic owns this state and mutates it via IPC/modes; the UI observes it.
//!
//! Exposed properties (conceptually):
//!   launcher.query
//!   launcher.results          // Vec<AppEntry>
//!   launcher.selected_index   // usize
//!   launcher.selected_result  // Option<AppEntry>
//!   launcher.mode             // not used (all modes combined)
//!   launcher.loading
//!   launcher.results_count

use hiren_shared::AppEntry;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct LauncherState {
    pub query: String,
    pub results: Vec<AppEntry>,
    pub selected_index: usize,
    pub loading: bool,
    pub error: Option<String>,
}

impl LauncherState {
    pub fn new() -> Self {
        Self { query: String::new(), results: Vec::new(), selected_index: 0, loading: false, error: None }
    }
    pub fn selected_result(&self) -> Option<&AppEntry> {
        self.results.get(self.selected_index)
    }
    pub fn results_count(&self) -> usize { self.results.len() }
    pub fn set_results(&mut self, results: Vec<AppEntry>) {
        self.results = results;
        if self.selected_index >= self.results.len() {
            self.selected_index = self.results.len().saturating_sub(1);
        }
        if self.results.is_empty() { self.selected_index = 0; }
    }
    pub fn select_next(&mut self, delta: i32) {
        if self.results.is_empty() { return; }
        let len = self.results.len() as i32;
        let cur = self.selected_index as i32;
        let nxt = (cur + delta).clamp(0, len - 1);
        self.selected_index = nxt as usize;
    }
    pub fn select(&mut self, idx: usize) {
        if idx < self.results.len() { self.selected_index = idx; }
    }
}

impl Default for LauncherState { fn default() -> Self { Self::new() } }

/// Observable wrapper: interior mutability + subscriber callbacks.
pub struct ObservableState {
    inner: RefCell<LauncherState>,
    subscribers: RefCell<Vec<Box<dyn Fn(&LauncherState)>>>,
}

impl ObservableState {
    pub fn new(initial: LauncherState) -> Rc<Self> {
        Rc::new(Self { inner: RefCell::new(initial), subscribers: RefCell::new(Vec::new()) })
    }
    pub fn get(&self) -> LauncherState { self.inner.borrow().clone() }
    pub fn with<R>(&self, f: impl FnOnce(&LauncherState) -> R) -> R { f(&self.inner.borrow()) }
    pub fn update(&self, f: impl FnOnce(&mut LauncherState)) {
        { let mut s = self.inner.borrow_mut(); f(&mut s); }
        self.notify();
    }
    pub fn subscribe(&self, cb: impl Fn(&LauncherState) + 'static) {
        self.subscribers.borrow_mut().push(Box::new(cb));
    }
    fn notify(&self) {
        let snap = self.inner.borrow().clone();
        for cb in self.subscribers.borrow().iter() { cb(&snap); }
    }
}

// ---------------------------------------------------------------------------
// Launcher actions — the minimal coherent API the UI can invoke.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum LauncherAction {
    SetQuery(String),
    MoveSelection(i32),
    SetSelection(usize),
    Activate(Option<String>), // prefix
    ActivateIndex(usize, Option<String>),
    Close,
    CopyToClipboard(String),
}

pub trait ActionHandler {
    fn handle(&self, action: LauncherAction);
}
