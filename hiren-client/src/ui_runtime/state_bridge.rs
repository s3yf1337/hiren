//! Bridge between launcher modes (search) and ObservableState.

use crate::config::LauncherConfig;
use crate::freq::FreqHistory;
use crate::launcher::ObservableState;
use crate::modes::{self, SearchMode, SearchResult};
use hiren_shared::{AppEntry, AppMode};
use std::collections::HashMap;
use std::rc::Rc;

pub struct SearchBridge {
    pub sources: HashMap<AppMode, Box<dyn SearchMode>>,
    freq: FreqHistory,
    weight: f64,
}

impl SearchBridge {
    pub fn new(config: &LauncherConfig) -> Self {
        let mut sources: HashMap<AppMode, Box<dyn SearchMode>> = HashMap::new();
        for mode in config.active_modes() {
            let mut inst = modes::create_mode(mode);
            inst.init(config);
            sources.insert(mode, inst);
        }
        let freq = FreqHistory::load();
        Self { sources, freq, weight: config.freq_weight }
    }

    pub fn search(&self, query: &str, state: &Rc<ObservableState>) {
        let query = query.trim();
        let mut calc_results = Vec::new();
        let mut window_results = Vec::new();
        let mut other = Vec::new();

        if self.sources.contains_key(&AppMode::Calc) {
            if let Some(calc) = self.sources.get(&AppMode::Calc) {
                if let SearchResult::Entries(entries) = calc.search(query) {
                    for e in entries { if !e.name.starts_with("Error") && !e.name.contains("Enter a math") { calc_results.push(e); } }
                }
            }
        }
        if self.sources.contains_key(&AppMode::Drun) {
            if let Some(d) = self.sources.get(&AppMode::Drun) {
                if let SearchResult::Entries(e) = d.search(query) { other.extend(e); }
            }
        }
        if self.sources.contains_key(&AppMode::Run) {
            if let Some(r) = self.sources.get(&AppMode::Run) {
                if let SearchResult::Entries(e) = r.search(query) { other.extend(e); }
            }
        }
        if self.sources.contains_key(&AppMode::Window) {
            if let Some(w) = self.sources.get(&AppMode::Window) {
                if let SearchResult::Entries(e) = w.search(query) { window_results.extend(e); }
            }
        }

        if query.is_empty() {
            let (freqs, mut rest) = self.freq.partition_by_freq(other);
            rest.sort_by(|a,b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            other = freqs; other.extend(rest);
        } else {
            other = self.freq.sort_by_frecency(other, self.weight);
        }

        let mut all = calc_results;
        all.extend(window_results);
        all.extend(other);

        state.update(|s| {
            s.set_results(all);
            s.query = query.to_string();
            s.loading = false;
        });
    }

    pub fn record_launch(&mut self, entry: &AppEntry) {
        self.freq.record_launch(&entry.exec);
    }
}
