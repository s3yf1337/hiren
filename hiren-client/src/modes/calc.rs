//! Режим calc — вычисление математических выражений прямо в строке поиска.
//!
//! Поддерживает: +, -, *, /, %, ^ (степень), sqrt(), sin(), cos(), tan(),
//! abs(), floor(), ceil(), round(), log(), ln(), exp(), pi, e,
//! скобки, унарный минус, числа с плавающей точкой.

use super::{exec_detached, SearchMode, SearchResult};
use crate::config::Config;
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
        // Иначе 6 знаков после запятой, обрезаем trailing zeros
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

    fn execute(&self, entry: &AppEntry, _config: &Config) {
        // Копируем результат в буфер обмена через wl-copy / xclip
        let result = &entry.exec;
        if result.is_empty() {
            return;
        }

        eprintln!("[hiren-client] Calc result: {result}");

        // Пробуем wl-copy (Wayland), затем xclip (X11)
        let copy_cmd = if crate::config::cmd_exists("wl-copy") {
            format!("printf '%s' '{}' | wl-copy", shell_escape(result))
        } else if crate::config::cmd_exists("xclip") {
            format!(
                "printf '%s' '{}' | xclip -selection clipboard",
                shell_escape(result)
            )
        } else {
            eprintln!("[hiren-client] No clipboard tool found (wl-copy/xclip)");
            return;
        };

        exec_detached(&copy_cmd);
    }
}

/// Экранировать строку для использования в одинарных кавычках shell.
fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
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
}
