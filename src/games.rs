use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Game {
    pub appid: u64,
    pub name: String,
}

const GAMES_JSON: &str = include_str!("../assets/games.json");

static GAMES: OnceLock<Vec<Game>> = OnceLock::new();

pub fn all() -> &'static [Game] {
    GAMES.get_or_init(|| {
        let raw = serde_json::from_str::<Vec<(u64, String)>>(GAMES_JSON).unwrap_or_default();
        let mut games: Vec<Game> = raw
            .into_iter()
            .map(|(appid, name)| Game { appid, name })
            .collect();
        games.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        games
    })
}

pub fn name_for(appid: u64) -> Option<&'static str> {
    all()
        .iter()
        .find(|game| game.appid == appid)
        .map(|game| game.name.as_str())
}

pub fn search(query: &str, limit: usize) -> Vec<&'static Game> {
    let needle = query.trim().to_lowercase();
    all()
        .iter()
        .filter(|game| {
            needle.is_empty()
                || game.name.to_lowercase().contains(&needle)
                || game.appid.to_string().contains(&needle)
        })
        .take(limit)
        .collect()
}

/// Resolve free-form text from the game field. Returns an app id and a label.
pub fn resolve_input(text: &str) -> (Option<u64>, String) {
    let text = text.trim();
    if text.is_empty() {
        return (None, String::new());
    }

    if let Some(start) = text.rfind('(') {
        if text.ends_with(')') {
            let inside = &text[start + 1..text.len() - 1];
            if let Ok(appid) = inside.parse::<u64>() {
                if let Some(name) = name_for(appid) {
                    return (Some(appid), format!("→ {name} (app {appid})"));
                }
                return (
                    Some(appid),
                    format!("→ App {appid} (unknown game — may not support workshop)"),
                );
            }
        }
    }

    if let Some(appid) = text.parse::<u64>().ok().filter(|value| *value > 0) {
        if let Some(name) = name_for(appid) {
            return (Some(appid), format!("→ {name} (app {appid})"));
        }
        return (
            Some(appid),
            format!("→ App {appid} (unknown game — may not support workshop)"),
        );
    }

    if let Some(appid) = crate::workshop_api::extract_app_id(text) {
        if let Some(name) = name_for(appid) {
            return (Some(appid), format!("→ {name} (app {appid})"));
        }
        return (
            Some(appid),
            format!("→ App {appid} (unknown game — may not support workshop)"),
        );
    }

    let exact: Vec<&Game> = all()
        .iter()
        .filter(|game| game.name.eq_ignore_ascii_case(text))
        .collect();
    if exact.len() == 1 {
        let game = exact[0];
        return (
            Some(game.appid),
            format!("→ {} (app {})", game.name, game.appid),
        );
    }

    let matches: Vec<&Game> = all()
        .iter()
        .filter(|game| {
            game.name.to_lowercase().contains(&text.to_lowercase())
                || text.to_lowercase().contains(&game.name.to_lowercase())
        })
        .collect();
    if matches.len() == 1 {
        let game = matches[0];
        return (
            Some(game.appid),
            format!("→ {} (app {})", game.name, game.appid),
        );
    }
    if matches.len() > 1 {
        let names = matches
            .iter()
            .take(4)
            .map(|game| game.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return (
            None,
            format!("Ambiguous: did you mean one of: {names} ...?"),
        );
    }

    (
        None,
        format!("No game found for '{text}'. Paste a workshop URL or app id."),
    )
}
