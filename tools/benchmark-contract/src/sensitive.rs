//! Поиск сведений, которые нельзя публиковать: пути домашних каталогов,
//! похожие на секреты строки и адреса электронной почты.
//!
//! Это консервативный фильтр, а не доказательство отсутствия секретов:
//! исполнитель не должен класть такие данные в результат вовсе, а проверка
//! ловит случайную утечку до публикации.

/// Причина, по которой строка не публикуется, или `None`.
pub fn finding(text: &str) -> Option<&'static str> {
    const HOME_PREFIXES: &[&str] = &["/home/", "/Users/", "/root/", "/var/home/"];
    if HOME_PREFIXES
        .iter()
        .any(|prefix| contains_path_prefix(text, prefix))
        || text.starts_with("~/")
        || text.contains(" ~/")
        || contains_windows_profile(text)
    {
        return Some("путь внутри домашнего каталога пользователя");
    }
    const SECRET_PREFIXES: &[&str] = &[
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xapp-",
        "sk-ant-",
        "-----BEGIN",
    ];
    if SECRET_PREFIXES.iter().any(|prefix| text.contains(prefix)) {
        return Some("строка похожа на токен или закрытый ключ");
    }
    if aws_access_key(text) {
        return Some("строка похожа на ключ доступа AWS");
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("bearer ")
        || [
            "password=",
            "passwd=",
            "token=",
            "secret=",
            "api_key=",
            "apikey=",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return Some("строка содержит учётные данные");
    }
    if email(text) {
        return Some("адрес электронной почты");
    }
    None
}

/// Имя переменной окружения или параметра, по которому обычно передают секрет.
pub fn secret_like_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "API_KEY",
        "APIKEY",
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}

fn contains_path_prefix(text: &str, prefix: &str) -> bool {
    text.match_indices(prefix).any(|(index, _)| {
        index == 0
            || !text[..index]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    })
}

fn contains_windows_profile(text: &str) -> bool {
    let lower = text.to_ascii_lowercase().replace('/', "\\");
    lower.contains(":\\users\\") || lower.contains("\\\\wsl") || contains_wsl_drive_profile(text)
}

/// `/mnt/<диск>/Users/…` — профиль Windows, смонтированный в WSL.
fn contains_wsl_drive_profile(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.match_indices("/mnt/").any(|(index, _)| {
        let mut rest = lower[index + 5..].chars();
        rest.next().is_some_and(|c| c.is_ascii_alphabetic()) && rest.as_str().starts_with("/users/")
    })
}

fn aws_access_key(text: &str) -> bool {
    text.match_indices("AKIA").any(|(index, _)| {
        let rest = &text[index + 4..];
        rest.len() >= 16
            && rest
                .bytes()
                .take(16)
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    })
}

fn email(text: &str) -> bool {
    text.split(|c: char| {
        c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')' | ',' | ';' | '"' | '\'')
    })
    .any(|word| {
        // Адрес в конце фразы: «пишите a@example.org.» или «a@example.org:».
        let word = word.trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?' | '[' | ']'));
        let Some((local, domain)) = word.split_once('@') else {
            return false;
        };
        !local.is_empty()
            && local
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c))
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && domain
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
            && domain
                .rsplit('.')
                .next()
                .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_home_paths_tokens_and_emails() {
        assert!(finding("/home/alice/decks").is_some());
        assert!(finding("archive at ~/Downloads/anki.tar").is_some());
        assert!(finding(r"C:\Users\Alice\anki").is_some());
        assert!(finding("/mnt/c/Users/Alice/anki").is_some());
        assert!(finding("token ghp_abcdefghijklmnop").is_some());
        assert!(finding("AKIAABCDEFGHIJKLMNOP").is_some());
        assert!(finding("Authorization: Bearer x").is_some());
        assert!(finding("mail me: someone@example.org").is_some());
        assert!(finding("пишите someone@example.org.").is_some());
        assert!(finding("contact someone@example.org:").is_some());
        assert!(finding("PASSWORD=hunter2").is_some());
    }

    #[test]
    fn keeps_ordinary_technical_strings() {
        for text in [
            "/sys/fs/cgroup/system.slice",
            "target/anki-e2e/cache",
            "WSL2 6.18 x86_64",
            "AMD Ryzen 7 5800X3D",
            "rust:1.99.1-bookworm",
            "anki-e2e@0.1.0",
            "time_wall_clock@command",
        ] {
            assert_eq!(finding(text), None, "{text}");
        }
        assert!(secret_like_name("GITHUB_TOKEN"));
        assert!(!secret_like_name("PATH"));
    }
}
