//! Bridge between launcher modes (search) and ObservableState.
//!
//! Единая точка, где результаты поиска попадают в [`LauncherState`]:
//! - `loading` выставляется до поиска и сбрасывается после;
//! - ошибки источников (`SearchResult::Error`, в первую очередь
//!   недоступный демон IPC) попадают в `LauncherState.error`, который
//!   темы показывают через binding `launcher.error`;
//! - успешный поиск очищает прошлую ошибку.

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

        // Поиск синхронный, но состояние должно проходить через «loading»
        // снимок: темы могут показать спиннер/скрыть список.
        state.update(|s| {
            s.query = query.to_string();
            s.loading = true;
            s.error = None;
        });

        let mut calc_results = Vec::new();
        let mut window_results = Vec::new();
        let mut other = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        if let Some(calc) = self.sources.get(&AppMode::Calc) {
            match calc.search(query) {
                SearchResult::Entries(entries) => {
                    for e in entries {
                        // Calc-режим возвращает подсказки как обычные записи;
                        // в общий список они не идут (только результаты).
                        if !e.name.starts_with("Error") && !e.name.contains("Enter a math") {
                            calc_results.push(e);
                        }
                    }
                }
                SearchResult::Error(e) => errors.push(format!("calc: {e}")),
            }
        }

        if let Some(d) = self.sources.get(&AppMode::Drun) {
            match d.search(query) {
                SearchResult::Entries(entries) => other.extend(entries),
                SearchResult::Error(e) => errors.push(format!("applications: {e}")),
            }
        }

        if let Some(r) = self.sources.get(&AppMode::Run) {
            match r.search(query) {
                SearchResult::Entries(entries) => other.extend(entries),
                SearchResult::Error(e) => errors.push(format!("run: {e}")),
            }
        }

        if let Some(w) = self.sources.get(&AppMode::Window) {
            match w.search(query) {
                SearchResult::Entries(entries) => window_results.extend(entries),
                SearchResult::Error(e) => errors.push(format!("windows: {e}")),
            }
        }

        if query.is_empty() {
            let (freqs, mut rest) = self.freq.partition_by_freq(other);
            rest.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            other = freqs;
            other.extend(rest);
        } else {
            other = self.freq.sort_by_frecency(other, self.weight);
        }

        let mut all = calc_results;
        all.extend(window_results);
        all.extend(other);

        let error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
        state.update(|s| {
            s.set_results(all);
            s.loading = false;
            s.error = error;
        });
    }

    pub fn record_launch(&mut self, entry: &AppEntry) {
        self.freq.record_launch(&entry.exec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LauncherConfig as Config;
    use crate::launcher::LauncherState;
    use std::cell::RefCell;

    /// Снимки подписчика: (loading, error).
    type Snapshots = Rc<RefCell<Vec<(bool, Option<String>)>>>;

    /// Фейковый источник: фиксированные результаты или ошибка.
    struct FakeMode {
        entries: Vec<AppEntry>,
        error: Option<String>,
    }

    impl SearchMode for FakeMode {
        fn search(&self, _query: &str) -> SearchResult {
            match &self.error {
                Some(error) => SearchResult::Error(error.clone()),
                None => SearchResult::Entries(self.entries.clone()),
            }
        }

        fn execute(&self, _entry: &AppEntry, _config: &Config) {}
    }

    fn bridge(sources: Vec<(AppMode, FakeMode)>) -> SearchBridge {
        SearchBridge {
            sources: sources
                .into_iter()
                .map(|(mode, source)| (mode, Box::new(source) as Box<dyn SearchMode>))
                .collect(),
            freq: FreqHistory::default(),
            weight: 0.8,
        }
    }

    fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry::run(id.to_string(), name.to_string(), format!("/usr/bin/{id}"))
    }

    #[test]
    fn mode_error_is_surfaced_in_state() {
        let state = ObservableState::new(LauncherState::new());
        let bridge = bridge(vec![(
            AppMode::Drun,
            FakeMode {
                entries: Vec::new(),
                error: Some("daemon unavailable".into()),
            },
        )]);

        bridge.search("fire", &state);
        let snapshot = state.get();
        assert_eq!(snapshot.query, "fire");
        assert!(!snapshot.loading, "loading must be reset after synchronous search");
        let error = snapshot.error.expect("error must be surfaced");
        assert!(
            error.contains("daemon unavailable"),
            "unexpected error text: {error}"
        );
        assert!(snapshot.results.is_empty());
    }

    #[test]
    fn successful_search_clears_previous_error() {
        let state = ObservableState::new(LauncherState::new());
        state.update(|s| s.error = Some("old error".into()));

        let bridge = bridge(vec![(
            AppMode::Drun,
            FakeMode {
                entries: vec![entry("firefox", "Firefox")],
                error: None,
            },
        )]);

        bridge.search("fire", &state);
        let snapshot = state.get();
        assert_eq!(snapshot.error, None);
        assert_eq!(snapshot.results.len(), 1);
        assert_eq!(snapshot.results[0].id, "firefox");
        assert!(!snapshot.loading);
    }

    #[test]
    fn loading_true_is_observable_before_results_arrive() {
        let state = ObservableState::new(LauncherState::new());
        let seen: Snapshots = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.subscribe(move |snapshot| {
                seen.borrow_mut().push((snapshot.loading, snapshot.error.clone()));
            });
        }
        let bridge = bridge(vec![(
            AppMode::Drun,
            FakeMode {
                entries: vec![entry("x", "X")],
                error: None,
            },
        )]);

        bridge.search("x", &state);
        let seen = seen.borrow();
        assert_eq!(seen.first(), Some(&(true, None)), "loading=true first");
        assert_eq!(seen.last(), Some(&(false, None)), "loading=false after results");
    }

    #[test]
    fn errors_from_multiple_modes_are_combined() {
        let state = ObservableState::new(LauncherState::new());
        let bridge = bridge(vec![
            (
                AppMode::Drun,
                FakeMode {
                    entries: Vec::new(),
                    error: Some("daemon down".into()),
                },
            ),
            (
                AppMode::Window,
                FakeMode {
                    entries: Vec::new(),
                    error: Some("wmctrl missing".into()),
                },
            ),
        ]);

        bridge.search("x", &state);
        let error = state.get().error.expect("error");
        assert!(error.contains("daemon down"), "error: {error}");
        assert!(error.contains("wmctrl missing"), "error: {error}");
    }

    #[test]
    fn empty_query_keeps_deterministic_name_order() {
        let state = ObservableState::new(LauncherState::new());
        let bridge = bridge(vec![(
            AppMode::Drun,
            FakeMode {
                entries: vec![entry("b", "Beta"), entry("a", "Alpha")],
                error: None,
            },
        )]);

        bridge.search("", &state);
        let names: Vec<String> = state.get().results.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["Alpha", "Beta"]);
    }

    #[test]
    fn calc_entries_do_not_leak_into_results_as_errors() {
        let state = ObservableState::new(LauncherState::new());
        let bridge = bridge(vec![(
            AppMode::Calc,
            FakeMode {
                entries: vec![
                    entry("calc", "1+1 = 2"),
                    AppEntry::calc_error("bad expression".into()),
                ],
                error: None,
            },
        )]);

        bridge.search("1+1", &state);
        let results = state.get().results;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "1+1 = 2");
    }
}
