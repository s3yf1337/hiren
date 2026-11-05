//! Режим calc — вычисление математических выражений прямо в строке поиска.
//!
//! Поддерживает: +, -, *, /, %, ^ (степень), sqrt(), sin(), cos(), tan(),
//! abs(), floor(), ceil(), round(), log(), ln(), exp(), pi, e,
//! скобки, унарный минус, числа с плавающей точкой.

use super::{clipboard_command, spawn_with_stdin, LaunchErrors, SearchMode, SearchResult};
use crate::config::LauncherConfig as Config;
use hiren_shared::AppEntry;

pub struct CalcMode;

impl CalcMode {
    pub fn new() -> Self {
        Self
    }

    /// Попытаться вычислить выражение.
    fn evaluate(expr: &str) -> Result<f64, String> {
        meval::eval_str(expr).map_err(|e| format!("Invalid expression: {e}"))
    }

    /// Форматировать результат: если целое — без десятичной части.
    fn format_result(value: f64) -> String {
        if value.is_nan() {
            return "NaN".into();
        }
        if value.is_infinite() {
            return if value > 0.0 { "∞" } else { "-∞" }.into();
        }
        // Если значение близко к целому — показываем без .0
        let rounded = value.round();
        if (value - rounded).abs() < 1e-10 {
            return format!("{}", rounded as i64);
        }
        // Иначе 10 знаков после запятой, обрезаем trailing zeros
        let s = format!("{:.10}", value);
        let s = s.trim_end_matches('0');
        let s = s.trim_end_matches('.');
        s.to_string()
    }
}

impl SearchMode for CalcMode {
    fn search(&self, query: &str) -> SearchResult {
        let query = query.trim();

        if query.is_empty() {
            return SearchResult::Entries(vec![AppEntry::calc_result(
                "".into(),
                "Enter a math expression…".into(),
            )]);
        }

        // Всегда пытаемся вычислить — даже без looks_like_expression,
        // потому что пользователь мог ввести "2+2"
        match Self::evaluate(query) {
            Ok(value) => {
                let formatted = Self::format_result(value);
                let entry = AppEntry::calc_result(query.to_string(), formatted);
                SearchResult::Entries(vec![entry])
            }
            Err(e) => {
                // Показываем ошибку как запись в списке
                let entry = AppEntry::calc_error(e);
                SearchResult::Entries(vec![entry])
            }
        }
    }

    fn execute(&self, entry: &AppEntry, _config: &Config, errors: &LaunchErrors) -> Result<(), String> {
        let result = &entry.exec;
        if result.is_empty() {
            return Err("calc result is empty".into());
        }

        eprintln!("[hiren-client] Calc result: {result}");

        // wl-copy на Wayland, xclip на X11. Результат уходит на stdin —
        // без shell и без экранирования кавычек в выражении.
        let argv = clipboard_command()
            .ok_or_else(|| "no clipboard tool found (wl-copy/xclip)".to_string())?;
        spawn_with_stdin(&argv, result, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_arithmetic() {
        assert!((CalcMode::evaluate("1+1").unwrap() - 2.0).abs() < 0.001);
        assert!((CalcMode::evaluate("2*3").unwrap() - 6.0).abs() < 0.001);
        assert!((CalcMode::evaluate("10/2").unwrap() - 5.0).abs() < 0.001);
        assert!((CalcMode::evaluate("5-3").unwrap() - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_search_returns_result() {
        let mode = CalcMode::new();
        let result = mode.search("1+1");
        match result {
            SearchResult::Entries(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(entries[0].name.contains("1+1"));
                assert!(entries[0].name.contains("2"));
            }
            SearchResult::Error(e) => panic!("Expected entries, got error: {e}"),
        }
    }

    #[test]
    fn test_format_result_integers_without_decimal_part() {
        assert_eq!(CalcMode::format_result(2.0), "2");
        assert_eq!(CalcMode::format_result(-7.0), "-7");
        assert_eq!(CalcMode::format_result(2.5), "2.5");
    }

    #[test]
    fn execute_without_result_is_error() {
        let entry = AppEntry::calc_error("bad".into());
        let config = Config::default();
        let errors = super::super::new_launch_errors();
        assert!(CalcMode::new().execute(&entry, &config, &errors).is_err());
    }
}
