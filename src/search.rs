//! 搜索与排序：模糊匹配（名称 / 全拼 / 拼音首字母）+ 使用频率加权。
//!
//! 排序口径与 PowerToys Run / Wox 类似：直接命中名称优先，其次拼音；常用项加权上浮。

use std::collections::HashMap;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use crate::apps::AppEntry;

/// 一条搜索结果。
pub struct Scored {
    /// 在应用列表中的下标
    pub index: usize,
    pub score: i64,
    /// 命中应用名时的字符下标（用于高亮；拼音命中为空）
    pub indices: Vec<usize>,
}

/// 执行搜索并返回按相关度排序的前 `max` 条。
pub fn search(
    matcher: &mut SkimMatcherV2,
    apps: &[AppEntry],
    usage: &HashMap<String, u64>,
    query: &str,
    max: usize,
) -> Vec<Scored> {
    let q = query.trim();
    if q.is_empty() {
        // 空查询：按使用次数排（常用在前），其余按名称
        let mut items: Vec<Scored> = apps
            .iter()
            .enumerate()
            .map(|(i, app)| Scored {
                index: i,
                score: usage.get(&app.key).copied().unwrap_or(0).min(1_000_000) as i64,
                indices: Vec::new(),
            })
            .collect();
        items.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| apps[a.index].name_lower.cmp(&apps[b.index].name_lower))
        });
        items.truncate(max);
        return items;
    }

    let q_lower = q.to_lowercase();
    let q_compact: String = q_lower.chars().filter(|c| !c.is_whitespace()).collect();
    // 拼音只对「字母/数字」查询启用：中文查询直接匹配名称
    let pinyin_query = !q_compact.is_empty() && q_compact.chars().all(|c| c.is_ascii_alphanumeric());

    let mut items: Vec<Scored> = Vec::with_capacity(apps.len());
    for (i, app) in apps.iter().enumerate() {
        let mut best: Option<(f64, Vec<usize>)> = None;

        if let Some((score, indices)) = matcher.fuzzy_indices(&app.name, q) {
            best = Some((score as f64, indices));
        }
        if pinyin_query {
            if let Some((score, _)) = matcher.fuzzy_indices(&app.py_full, &q_compact) {
                let weighted = score as f64 * 0.92;
                if best.as_ref().is_none_or(|(s, _)| weighted > *s) {
                    best = Some((weighted, Vec::new()));
                }
            }
            if let Some((score, _)) = matcher.fuzzy_indices(&app.py_short, &q_compact) {
                let weighted = score as f64 * 0.90;
                if best.as_ref().is_none_or(|(s, _)| weighted > *s) {
                    best = Some((weighted, Vec::new()));
                }
            }
        }

        let Some((base, indices)) = best else {
            continue;
        };

        let mut score = base;
        if app.name_lower == q_lower {
            score += 5000.0;
        } else if app.name_lower.starts_with(&q_lower) {
            score += 800.0;
        } else if app.py_short.starts_with(&q_compact) || app.py_full.starts_with(&q_compact) {
            score += 300.0;
        }
        // 常用项加权（对数增长，避免高频项永久霸榜）
        let used = usage.get(&app.key).copied().unwrap_or(0);
        score += (used as f64).ln_1p() * 60.0;
        // 同等匹配下名字短的更可能是用户想要的
        score -= app.name.chars().count() as f64 * 0.5;

        items.push(Scored {
            index: i,
            score: score as i64,
            indices,
        });
    }

    items.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| apps[a.index].name_lower.cmp(&apps[b.index].name_lower))
    });
    items.truncate(max);
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::{AppEntry, LaunchTarget};

    fn app(name: &str, py_full: &str, py_short: &str) -> AppEntry {
        AppEntry {
            name: name.to_string(),
            key: format!("k:{name}"),
            target: LaunchTarget::Path(format!("C:\\{name}.lnk")),
            icon: None,
            name_lower: name.to_lowercase(),
            py_full: py_full.to_string(),
            py_short: py_short.to_string(),
        }
    }

    fn fixture() -> Vec<AppEntry> {
        vec![
            app("微信", "weixin", "wx"),
            app("微信输入法", "weixinshurufa", "wxsrf"),
            app("Visual Studio Code", "visual studio code", "visual studio code"),
            app("计算器", "jisuanqi", "jsq"),
        ]
    }

    #[test]
    fn matches_name_and_pinyin() {
        let mut m = SkimMatcherV2::default().ignore_case();
        let apps = fixture();
        let usage = HashMap::new();

        let r = search(&mut m, &apps, &usage, "微信", 10);
        assert_eq!(r.len(), 2, "两个微信相关应用都应命中");
        assert_eq!(apps[r[0].index].name, "微信", "精确名优先");

        let r = search(&mut m, &apps, &usage, "wx", 10);
        assert!(r.len() >= 2);
        let names: Vec<&str> = r.iter().map(|s| apps[s.index].name.as_str()).collect();
        assert!(names.contains(&"微信"));
        assert!(names.contains(&"微信输入法"));

        let r = search(&mut m, &apps, &usage, "jsq", 10);
        assert_eq!(apps[r[0].index].name, "计算器");

        let r = search(&mut m, &apps, &usage, "vsc", 10);
        assert_eq!(apps[r[0].index].name, "Visual Studio Code");
    }

    #[test]
    fn empty_query_orders_by_usage() {
        let mut m = SkimMatcherV2::default().ignore_case();
        let apps = fixture();
        let mut usage = HashMap::new();
        usage.insert("k:计算器".to_string(), 9);
        let r = search(&mut m, &apps, &usage, "  ", 10);
        assert_eq!(apps[r[0].index].name, "计算器");
    }

    #[test]
    fn max_results_is_respected() {
        let mut m = SkimMatcherV2::default().ignore_case();
        let apps = fixture();
        let r = search(&mut m, &apps, &HashMap::new(), "", 2);
        assert_eq!(r.len(), 2);
    }
}
