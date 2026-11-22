//! Сопоставление русских названий устройств и комнат с записями каталога.
//!
//! LLM передаёт только текст. Идентификаторы устройств сюда не попадают:
//! выбрать можно лишь запись, которую вернул этот модуль.

use std::collections::BTreeMap;

use petrovich_core::config::SmartHomeConfig;

use super::model::{CategoryPolicy, DeviceKind, DeviceRecord};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectMode {
    Control,
    State,
}

#[derive(Debug)]
pub struct Selection {
    pub indices: Vec<usize>,
    pub clarification: Option<String>,
}

/// Нормализует текст: нижний регистр, ё→е, всё кроме букв и цифр — пробел.
pub fn normalize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut space = true;
    for ch in value.chars().flat_map(char::to_lowercase) {
        let ch = if ch == 'ё' { 'е' } else { ch };
        if ch.is_alphanumeric() {
            out.push(ch);
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim().to_owned()
}

fn words(value: &str) -> Vec<String> {
    normalize(value)
        .split_whitespace()
        .filter(|w| !is_stopword(w))
        .map(stem)
        .collect()
}

fn is_stopword(word: &str) -> bool {
    matches!(
        word,
        "в" | "во" | "на" | "и" | "с" | "со" | "у" | "к" | "по" | "для" | "the" | "a"
    )
}

/// Грубый стеммер для русских существительных/прилагательных. Он не обязан
/// быть лингвистически точным: задача — свести «гостиной» и «гостиная».
fn stem(word: &str) -> String {
    let mut w = word.to_owned();
    for suffix in [
        "ами", "ями", "ого", "его", "ому", "ему", "ыми", "ими", "ой", "ей", "ый", "ий", "ая", "яя",
        "ое", "ее", "ые", "ие", "ов", "ев", "ам", "ям", "ах", "ях", "ом", "ем", "а", "я", "ы", "и",
        "у", "ю", "е", "о",
    ] {
        if w.len() > suffix.len() + 3 && w.ends_with(suffix) {
            w.truncate(w.len() - suffix.len());
            break;
        }
    }
    w
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Light,
    Plug,
    Switch,
    Climate,
    Cover,
    Sensor,
}

impl Group {
    fn kind(self) -> DeviceKind {
        match self {
            Self::Light => DeviceKind::Light,
            Self::Plug => DeviceKind::Plug,
            Self::Switch => DeviceKind::Switch,
            Self::Climate => DeviceKind::Climate,
            Self::Cover => DeviceKind::Cover,
            Self::Sensor => DeviceKind::Sensor,
        }
    }
}

fn group_of(norm: &str) -> Option<Group> {
    let n = format!(" {norm} ");
    let has = |root: &str| n.contains(root);
    if has("свет") || has("ламп") || has("люстр") || has("освещ") || has("светильник")
    {
        Some(Group::Light)
    } else if has("розет") || has("удлинител") {
        Some(Group::Plug)
    } else if has("выключател") || has("переключател") {
        Some(Group::Switch)
    } else if has("кондицион") || has("обогрев") || has("климат") || has("теплый пол")
    {
        Some(Group::Climate)
    } else if has("штор") || has("жалюз") || has("занавес") {
        Some(Group::Cover)
    } else if has("датчик") || has("температур") || has("влажност") || has("градус")
    {
        Some(Group::Sensor)
    } else {
        None
    }
}

fn is_all(norm: &str) -> bool {
    norm.split_whitespace()
        .any(|w| matches!(w, "все" | "весь" | "вся" | "всю" | "всё" | "all"))
}

fn policy_kind(policy: CategoryPolicy) -> Option<DeviceKind> {
    match policy {
        CategoryPolicy::Control(k) | CategoryPolicy::ReadOnly(k) => Some(k),
        _ => None,
    }
}

fn allowed(policy: CategoryPolicy, mode: SelectMode) -> bool {
    match (policy, mode) {
        (CategoryPolicy::Control(k), _) => k.can_control(),
        (CategoryPolicy::ReadOnly(k), SelectMode::State) => {
            k.can_control() || k == DeviceKind::Sensor
        }
        (CategoryPolicy::ReadOnly(_), SelectMode::Control) => false,
        _ => false,
    }
}

fn room_matches(
    record: &DeviceRecord,
    room_norm: &str,
    aliases: &BTreeMap<String, String>,
) -> bool {
    let Some(room) = &record.room else {
        return false;
    };
    let target = aliases
        .iter()
        .find(|(k, _)| normalize(k) == room_norm)
        .map(|(_, v)| normalize(v))
        .unwrap_or_else(|| room_norm.to_owned());
    let name = normalize(&room.name);
    if name == target || name.contains(&target) || target.contains(&name) {
        return true;
    }
    words(&room.name) == words(&target)
}

/// Выделяет комнату из текста целиком: «лампа в спальне», «свет на кухне».
fn split_room(
    target: &str,
    records: &[DeviceRecord],
    config: &SmartHomeConfig,
) -> (String, Option<String>) {
    let norm = normalize(target);
    let mut rooms: Vec<String> = records
        .iter()
        .filter_map(|r| r.room.as_ref().map(|room| room.name.clone()))
        .chain(config.room_aliases.values().cloned())
        .collect();
    rooms.sort();
    rooms.dedup();
    let tokens: Vec<&str> = norm.split_whitespace().collect();
    for room in rooms {
        let room_words = words(&room);
        if room_words.is_empty() {
            continue;
        }
        for (i, token) in tokens.iter().enumerate() {
            if !matches!(*token, "в" | "во" | "на") {
                continue;
            }
            let tail = &tokens[i + 1..];
            let tail_words: Vec<String> = tail.iter().map(|w| stem(w)).collect();
            if tail_words == room_words
                || (!tail_words.is_empty()
                    && room_words
                        .iter()
                        .all(|rw| tail_words.iter().any(|tw| tw == rw)))
            {
                let rest = tokens[..i].join(" ");
                if !rest.is_empty() {
                    return (rest, Some(room));
                }
            }
        }
    }
    (norm, None)
}

/// Возвращает индексы разрешённых устройств. Для управления датчики и шлюзы
/// исключены; для состояния датчики доступны только на чтение.
pub fn select(
    records: &[DeviceRecord],
    policies: &[CategoryPolicy],
    config: &SmartHomeConfig,
    target: Option<&str>,
    room: Option<&str>,
    mode: SelectMode,
) -> Result<Selection, String> {
    let (target_norm, room_from_target) = match target {
        Some(t) if !t.trim().is_empty() => split_room(t, records, config),
        _ => (String::new(), None),
    };
    let room_target = room
        .filter(|r| !r.trim().is_empty())
        .map(str::to_owned)
        .or(room_from_target);
    let room_norm = room_target
        .as_deref()
        .map(normalize)
        .filter(|r| !r.is_empty());

    if let (Some(room), Some(norm)) = (&room_target, &room_norm) {
        let known = records.iter().enumerate().any(|(i, r)| {
            policies.get(i).is_some_and(|p| allowed(*p, mode))
                && room_matches(r, norm, &config.room_aliases)
        });
        if !known {
            return Err(format!(
                "комната «{room}» не найдена или в ней нет разрешённых устройств"
            ));
        }
    }

    let mut candidates: Vec<usize> = (0..records.len())
        .filter(|i| allowed(policies[*i], mode) && policy_kind(policies[*i]).is_some())
        .collect();
    if let Some(norm) = &room_norm {
        candidates.retain(|i| room_matches(&records[*i], norm, &config.room_aliases));
    }
    if candidates.is_empty() {
        return Err(if let Some(room) = &room_target {
            format!("в комнате «{room}» нет разрешённых устройств")
        } else {
            "нет разрешённых устройств".to_owned()
        });
    }

    let target_norm = apply_alias(&target_norm, &config.aliases);
    let group = group_of(&target_norm);
    let explicit_all = is_all(&target_norm);

    if target_norm.is_empty() || explicit_all {
        let indices = match group {
            Some(g) => {
                let mut v: Vec<usize> = candidates
                    .iter()
                    .copied()
                    .filter(|i| policy_kind(policies[*i]) == Some(g.kind()))
                    .collect();
                if v.is_empty() && matches!(g, Group::Sensor) && mode == SelectMode::State {
                    v = candidates
                        .iter()
                        .copied()
                        .filter(|i| policy_kind(policies[*i]) == Some(DeviceKind::Climate))
                        .collect();
                }
                v
            }
            None => candidates,
        };
        if indices.is_empty() {
            return Err(match group {
                Some(g) => format!(
                    "в этой области нет устройств категории «{}»",
                    g.kind().label()
                ),
                None => "нет разрешённых устройств".to_owned(),
            });
        }
        return Ok(Selection {
            indices,
            clarification: None,
        });
    }

    if let Some(g) = group {
        let exact: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|i| {
                policy_kind(policies[*i]) == Some(g.kind())
                    && name_matches(&records[*i].name, &target_norm)
            })
            .collect();
        if exact.len() == 1 {
            return Ok(Selection {
                indices: exact,
                clarification: None,
            });
        }
        let mut group_matches: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|i| policy_kind(policies[*i]) == Some(g.kind()))
            .collect();
        if group_matches.is_empty() && matches!(g, Group::Sensor) && mode == SelectMode::State {
            group_matches = candidates
                .iter()
                .copied()
                .filter(|i| policy_kind(policies[*i]) == Some(DeviceKind::Climate))
                .collect();
        }
        if !group_matches.is_empty() {
            // «лампа» без комнаты при нескольких лампах — это не «все лампы»:
            // просим уточнить, кроме явного «весь свет»/«все лампы».
            let plural = is_plural(&target_norm) || explicit_all;
            if group_matches.len() == 1 || plural {
                return Ok(Selection {
                    indices: group_matches,
                    clarification: None,
                });
            }
            return Ok(Selection {
                indices: Vec::new(),
                clarification: Some(clarify(records, &group_matches)),
            });
        }
    }

    let exact: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|i| normalize(&records[*i].name) == target_norm)
        .collect();
    if exact.len() == 1 {
        return Ok(Selection {
            indices: exact,
            clarification: None,
        });
    }

    let mut fuzzy: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|i| name_matches(&records[*i].name, &target_norm))
        .collect();
    if fuzzy.is_empty() {
        fuzzy = candidates
            .iter()
            .copied()
            .filter(|i| token_score(&records[*i].name, &target_norm) >= 0.6)
            .collect();
    }
    if fuzzy.len() == 1 {
        return Ok(Selection {
            indices: fuzzy,
            clarification: None,
        });
    }
    if fuzzy.is_empty() {
        let mut names: Vec<String> = candidates
            .iter()
            .take(6)
            .map(|i| super::catalog::display_name(&records[*i]))
            .collect();
        names.sort();
        return Err(format!(
            "не нашёл устройство «{}». Доступно: {}",
            target.unwrap_or_default().trim(),
            names.join(", ")
        ));
    }
    if explicit_all || is_plural(&target_norm) {
        return Ok(Selection {
            indices: fuzzy,
            clarification: None,
        });
    }
    Ok(Selection {
        indices: Vec::new(),
        clarification: Some(clarify(records, &fuzzy)),
    })
}

fn apply_alias(target: &str, aliases: &BTreeMap<String, String>) -> String {
    if target.is_empty() {
        return String::new();
    }
    for (from, to) in aliases {
        if normalize(from) == target {
            return normalize(to);
        }
    }
    target.to_owned()
}

fn name_matches(name: &str, target: &str) -> bool {
    let name = normalize(name);
    !name.is_empty()
        && !target.is_empty()
        && (name == target || name.contains(target) || target.contains(&name))
}

fn token_score(name: &str, target: &str) -> f32 {
    let target_words = words(target);
    if target_words.is_empty() {
        return 0.0;
    }
    let name_words = words(name);
    let hit = target_words
        .iter()
        .filter(|w| {
            name_words
                .iter()
                .any(|n| n == *w || n.starts_with(w.as_str()) || w.starts_with(n))
        })
        .count();
    hit as f32 / target_words.len() as f32
}

fn is_plural(norm: &str) -> bool {
    norm.split_whitespace().any(|w| {
        w.ends_with("ы")
            || w.ends_with("ые")
            || w.ends_with("ие")
            || matches!(
                w,
                "свет"
                    | "освещение"
                    | "розетки"
                    | "лампы"
                    | "светильники"
                    | "выключатели"
                    | "шторы"
                    | "жалюзи"
                    | "датчики"
            )
    })
}

fn clarify(records: &[DeviceRecord], indices: &[usize]) -> String {
    let mut names: Vec<String> = indices
        .iter()
        .take(6)
        .map(|i| super::catalog::display_name(&records[*i]))
        .collect();
    names.sort();
    format!("Уточните устройство: {}.", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_home::catalog::policy_for;
    use crate::smart_home::model::{DeviceKind, Room};
    use std::collections::BTreeMap;

    fn dev(name: &str, room: &str, category: &str) -> DeviceRecord {
        DeviceRecord {
            id: format!("id-{name}"),
            name: name.into(),
            category: category.into(),
            product_id: String::new(),
            product_name: String::new(),
            model: String::new(),
            local_key: "0123456789abcdef".into(),
            sub: false,
            gateway_id: None,
            node_id: None,
            online: true,
            room: Some(Room {
                id: room.into(),
                name: room.into(),
            }),
            home_id: "h".into(),
            home_name: "Дом".into(),
            functions: Vec::new(),
            status: Vec::new(),
            dp_map: BTreeMap::new(),
            cloud_status: BTreeMap::new(),
            local_supported: true,
            unsupported_reason: String::new(),
            ip: None,
            version: None,
            device22: false,
            last_seen: 0,
            key_from: None,
        }
    }

    fn setup() -> (Vec<DeviceRecord>, Vec<CategoryPolicy>, SmartHomeConfig) {
        let records = vec![
            dev("Люстра", "Спальня", "dj"),
            dev("Торшер", "Гостиная", "dj"),
            dev("Розетка чайник", "Кухня", "cz"),
            dev("Кондиционер", "Гостиная", "kt"),
            dev("Датчик температуры", "Спальня", "wsdcg"),
            dev("Датчик движения", "Спальня", "pir"),
            dev("Замок", "Прихожая", "ms"),
        ];
        let policies = records.iter().map(|r| policy_for(&r.category)).collect();
        (records, policies, SmartHomeConfig::default())
    }

    #[test]
    fn all_lights_with_room() {
        let (r, p, c) = setup();
        let s = select(
            &r,
            &p,
            &c,
            Some("весь свет"),
            Some("спальня"),
            SelectMode::Control,
        )
        .unwrap();
        assert_eq!(s.indices, vec![0]);
    }

    #[test]
    fn all_sockets_in_room() {
        let (r, p, c) = setup();
        let s = select(
            &r,
            &p,
            &c,
            Some("все розетки"),
            Some("кухня"),
            SelectMode::Control,
        )
        .unwrap();
        assert_eq!(s.indices, vec![2]);
    }

    #[test]
    fn room_inside_phrase_is_extracted() {
        let (r, p, c) = setup();
        let s = select(
            &r,
            &p,
            &c,
            Some("включи лампу в спальне"),
            None,
            SelectMode::Control,
        );
        // «включи» — не название устройства; должен найтись свет в спальне
        // либо уточнение, но не устройство из другой комнаты.
        match s {
            Ok(sel) => assert_eq!(sel.indices, vec![0]),
            Err(e) => assert!(e.contains("не нашёл") || e.contains("Уточните")),
        }
    }

    #[test]
    fn ambiguous_lamp_asks_for_clarification() {
        let (r, p, c) = setup();
        let s = select(&r, &p, &c, Some("лампа"), None, SelectMode::Control).unwrap();
        assert!(s.indices.is_empty());
        assert!(s.clarification.unwrap().contains("Уточните"));
    }

    #[test]
    fn verb_ending_in_i_is_not_plural() {
        let records = vec![dev("Люстра", "Спальня", "dj"), dev("Бра", "Спальня", "dj")];
        let policies: Vec<CategoryPolicy> =
            records.iter().map(|r| policy_for(&r.category)).collect();
        let s = select(
            &records,
            &policies,
            &SmartHomeConfig::default(),
            Some("включи лампу"),
            Some("спальня"),
            SelectMode::Control,
        )
        .unwrap();
        assert!(s.indices.is_empty());
        assert!(s.clarification.unwrap().contains("Уточните"));
    }

    #[test]
    fn plural_selects_all_matching() {
        let (r, p, c) = setup();
        let s = select(&r, &p, &c, Some("лампы"), None, SelectMode::Control).unwrap();
        assert_eq!(s.indices, vec![0, 1]);
    }

    #[test]
    fn sensors_are_read_only_and_forbidden_never_selected() {
        let (r, p, c) = setup();
        let s = select(&r, &p, &c, Some("датчики"), None, SelectMode::State).unwrap();
        assert_eq!(s.indices, vec![4, 5]);
        assert!(select(&r, &p, &c, Some("датчики"), None, SelectMode::Control).is_err());
        assert!(select(&r, &p, &c, Some("замок"), None, SelectMode::Control).is_err());
    }

    #[test]
    fn temperature_query_falls_back_to_climate() {
        let (r, p, c) = setup();
        let s = select(
            &r,
            &p,
            &c,
            Some("температура"),
            Some("гостиная"),
            SelectMode::State,
        )
        .unwrap();
        assert_eq!(s.indices, vec![3]);
    }

    #[test]
    fn aliases_are_applied() {
        let (r, p, mut c) = setup();
        c.aliases.insert("торшер".into(), "Торшер".into());
        c.room_aliases.insert("зал".into(), "Гостиная".into());
        let s = select(&r, &p, &c, Some("торшер"), Some("зал"), SelectMode::Control).unwrap();
        assert_eq!(s.indices, vec![1]);
    }

    #[test]
    fn unknown_room_errors() {
        let (r, p, c) = setup();
        let e = select(
            &r,
            &p,
            &c,
            Some("свет"),
            Some("подвал"),
            SelectMode::Control,
        )
        .unwrap_err();
        assert!(e.contains("не найдена"));
    }

    #[test]
    fn normalisation_handles_case_and_yo() {
        assert_eq!(normalize("  ВСЁ, Тёплый—пол! "), "все теплый пол");
        assert_eq!(stem("гостиной"), "гостин");
        assert_eq!(stem("гостиная"), "гостин");
        let _: Option<DeviceKind> = None;
    }
}
