//! Navi's hands on the user's own PC: the browser, web search, social accounts,
//! email, files, apps and a few system controls.
//!
//! Problem: "search for flights to Cairo" should open a browser whether or not
//! any model is running, and a model must never choose what gets executed.
//!
//! So the words are parsed here into a typed action, and only registered things
//! run: a URL built from a fixed table, a `.desktop` entry that exists, a file
//! the search found. Parsing has no side effects, because `run_command` reads
//! every clause more than once; only `execute` acts.
//!
//! Two phases keep existing commands first. `early` takes verbs nothing else
//! claims ("search", "google", "check my email"). `late` runs after the session
//! and progress commands, so a session called "Reddit" wins over the website.
use crate::{App, Result, adapters, now};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// (names, search URL with `{}` for the query, the name said in the reply).
const ENGINES: &[(&[&str], &str, &str)] = &[
    (&["google", "the web", "web", "online", "the internet", "internet"], "https://www.google.com/search?q={}", "Google"),
    (&["google images", "images"], "https://www.google.com/search?tbm=isch&q={}", "Google Images"),
    (&["google maps", "maps"], "https://www.google.com/maps/search/{}", "Google Maps"),
    (&["youtube"], "https://www.youtube.com/results?search_query={}", "YouTube"),
    (&["github"], "https://github.com/search?q={}", "GitHub"),
    (&["reddit"], "https://www.reddit.com/search/?q={}", "Reddit"),
    (&["amazon"], "https://www.amazon.com/s?k={}", "Amazon"),
    (&["wikipedia"], "https://en.wikipedia.org/w/index.php?search={}", "Wikipedia"),
    (&["duckduckgo", "duck duck go"], "https://duckduckgo.com/?q={}", "DuckDuckGo"),
    (&["bing"], "https://www.bing.com/search?q={}", "Bing"),
    (&["x", "twitter"], "https://x.com/search?q={}", "X"),
    (&["linkedin", "linked in"], "https://www.linkedin.com/search/results/all/?keywords={}", "LinkedIn"),
    (&["facebook"], "https://www.facebook.com/search/top?q={}", "Facebook"),
    (&["tiktok", "tik tok"], "https://www.tiktok.com/search?q={}", "TikTok"),
    (&["stack overflow", "stackoverflow"], "https://stackoverflow.com/search?q={}", "Stack Overflow"),
    (&["npm"], "https://www.npmjs.com/search?q={}", "npm"),
];

struct Site {
    names: &'static [&'static str],
    label: &'static str,
    home: &'static str,
    notifications: Option<&'static str>,
    messages: Option<&'static str>,
    /// The compose page that takes the text as a query; nothing posts until the
    /// user presses the site's own button.
    compose: Option<&'static str>,
}

const fn site(names: &'static [&'static str], label: &'static str, home: &'static str) -> Site {
    Site { names, label, home, notifications: None, messages: None, compose: None }
}

const SITES: &[Site] = &[
    Site { names: &["x", "twitter"], label: "X", home: "https://x.com/home",
        notifications: Some("https://x.com/notifications"), messages: Some("https://x.com/messages"),
        compose: Some("https://x.com/intent/post?text={}") },
    Site { names: &["facebook"], label: "Facebook", home: "https://www.facebook.com/",
        notifications: Some("https://www.facebook.com/notifications"), messages: Some("https://www.messenger.com/"), compose: None },
    Site { names: &["instagram", "insta"], label: "Instagram", home: "https://www.instagram.com/",
        notifications: Some("https://www.instagram.com/accounts/activity/"), messages: Some("https://www.instagram.com/direct/inbox/"), compose: None },
    Site { names: &["linkedin", "linked in"], label: "LinkedIn", home: "https://www.linkedin.com/feed/",
        notifications: Some("https://www.linkedin.com/notifications/"), messages: Some("https://www.linkedin.com/messaging/"),
        compose: Some("https://www.linkedin.com/feed/?shareActive=true&text={}") },
    Site { names: &["reddit"], label: "Reddit", home: "https://www.reddit.com/",
        notifications: Some("https://www.reddit.com/notifications"), messages: Some("https://www.reddit.com/message/inbox"),
        compose: Some("https://www.reddit.com/submit?title={}") },
    Site { names: &["threads"], label: "Threads", home: "https://www.threads.net/",
        notifications: Some("https://www.threads.net/activity"), messages: None,
        compose: Some("https://www.threads.net/intent/post?text={}") },
    Site { names: &["bluesky", "blue sky"], label: "Bluesky", home: "https://bsky.app/",
        notifications: Some("https://bsky.app/notifications"), messages: Some("https://bsky.app/messages"),
        compose: Some("https://bsky.app/intent/compose?text={}") },
    Site { names: &["github"], label: "GitHub", home: "https://github.com/",
        notifications: Some("https://github.com/notifications"), messages: None, compose: None },
    Site { names: &["youtube"], label: "YouTube", home: "https://www.youtube.com/",
        notifications: Some("https://www.youtube.com/feed/subscriptions"), messages: None, compose: None },
    site(&["tiktok", "tik tok"], "TikTok", "https://www.tiktok.com/"),
    site(&["whatsapp", "whats app"], "WhatsApp", "https://web.whatsapp.com/"),
    site(&["telegram"], "Telegram", "https://web.telegram.org/"),
    site(&["discord"], "Discord", "https://discord.com/app"),
    site(&["messenger"], "Messenger", "https://www.messenger.com/"),
    site(&["gmail", "google mail"], "Gmail", "https://mail.google.com/"),
    site(&["outlook", "hotmail"], "Outlook", "https://outlook.live.com/mail/"),
    site(&["yahoo mail"], "Yahoo Mail", "https://mail.yahoo.com/"),
    site(&["proton mail", "protonmail"], "Proton Mail", "https://mail.proton.me/"),
    site(&["google calendar", "calendar"], "Google Calendar", "https://calendar.google.com/"),
    site(&["google drive", "drive"], "Google Drive", "https://drive.google.com/"),
    site(&["google docs"], "Google Docs", "https://docs.google.com/"),
    site(&["google maps", "maps"], "Google Maps", "https://www.google.com/maps"),
    site(&["google"], "Google", "https://www.google.com/"),
    site(&["wikipedia"], "Wikipedia", "https://www.wikipedia.org/"),
    site(&["amazon"], "Amazon", "https://www.amazon.com/"),
    site(&["netflix"], "Netflix", "https://www.netflix.com/"),
    site(&["spotify"], "Spotify", "https://open.spotify.com/"),
    site(&["chatgpt", "chat gpt"], "ChatGPT", "https://chatgpt.com/"),
];

/// Folders a spoken project name is looked for in, under the home directory.
const PROJECT_ROOTS: &[&str] = &["Documents", "Projects", "projects", "code", "Code", "src", "dev", "work", "repos", "git"];
const PROJECT_MARKERS: &[&str] = &[".git", "package.json", "Cargo.toml", "pyproject.toml", "go.mod", "pom.xml", "Makefile"];
const SKIP_DIRS: &[&str] = &["node_modules", "target", "__pycache__", "site-packages", "venv", "dist", "build"];

/// Courtesy words around a command: the stripped text keeps the user's casing,
/// because the rest of it may be a search query or an email body.
fn trimmed(text: &str) -> String {
    const LEADS: &[&str] = &["hey navi", "okay navi", "ok navi", "navi", "i want you to", "i need you to", "i want to",
        "i need to", "i'd like to", "i would like to", "let's", "lets", "just", "quickly", "go ahead and", "please",
        "can you", "could you", "would you", "will you"];
    const TAILS: &[&str] = &[" for me", " please", " right now", " now", " navi"];
    let mut current = text.trim().trim_end_matches(['.', '!', '?', ' ']);
    'lead: loop {
        for lead in LEADS {
            if let Some(rest) = strip_ci(current, lead)
                && (rest.is_empty() || rest.starts_with([' ', ',', ':']))
            {
                current = rest.trim_start_matches([' ', ',', ':']);
                continue 'lead;
            }
        }
        break;
    }
    'tail: loop {
        for tail in TAILS {
            let lower = current.to_ascii_lowercase();
            if lower.ends_with(tail) {
                current = current[..current.len() - tail.len()].trim_end_matches([' ', ',']);
                continue 'tail;
            }
        }
        return current.to_string();
    }
}

/// ASCII-only case folding keeps byte offsets valid in any language's text.
fn strip_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

fn after_word<'a>(text: &'a str, words: &[&str]) -> Option<&'a str> {
    words.iter().find_map(|word| {
        let rest = strip_ci(text, word)?;
        rest.starts_with(' ').then(|| rest.trim_start())
    })
}

fn rsplit_ci<'a>(text: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
    let at = text.to_ascii_lowercase().rfind(marker)?;
    Some((text[..at].trim(), text[at + marker.len()..].trim()))
}

fn split_ci<'a>(text: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
    let at = text.to_ascii_lowercase().find(marker)?;
    Some((text[..at].trim(), text[at + marker.len()..].trim()))
}

pub fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn engine(name: &str) -> Option<&'static (&'static [&'static str], &'static str, &'static str)> {
    let name = name.trim().to_ascii_lowercase();
    ENGINES.iter().find(|entry| entry.0.contains(&name.as_str()))
}

fn search_action(query: &str, found: &(&[&str], &str, &str)) -> Option<Value> {
    let query = query.trim().trim_matches(['"', '“', '”', '\'']).trim();
    if query.is_empty() {
        return None;
    }
    Some(json!({"kind":"desk","op":"web","url":found.1.replace("{}", &url_encode(query)),
        "say":format!("Searching {} for {query}.", found.2)}))
}

/// "search for cats", "google cats", "look up cats on youtube", "play jazz on
/// youtube". Work on the code ("search the project for TODO") stays with the
/// assistant, which can read it.
fn parse_search(text: &str) -> Option<Value> {
    let lower = text.to_ascii_lowercase();
    const LOCAL: &[&str] = &[" code", " project", " repo", " repository", " codebase", " files", " folder", " directory", " file"];
    if LOCAL.iter().any(|word| lower.ends_with(word)) {
        return None;
    }
    let google = engine("google")?;
    // "play jazz on youtube", "watch cats on youtube".
    if let Some(rest) = after_word(text, &["play", "watch"])
        && let Some((query, place)) = rsplit_ci(rest, " on ")
        && place.eq_ignore_ascii_case("youtube")
    {
        return search_action(query, engine("youtube")?);
    }
    let rest = after_word(text, &["search", "web search", "look", "find"]);
    if let Some(rest) = rest {
        // "search youtube for cats", "search the web for cats".
        if let Some((place, query)) = split_ci(rest, " for ")
            && let Some(found) = engine(place)
        {
            return search_action(query, found);
        }
        let bare = strip_ci(rest, "for ").or_else(|| strip_ci(rest, "up ")).or_else(|| strip_ci(rest, "online for "));
        let query = bare.unwrap_or(rest).trim();
        // "search for cats on youtube", "find cats on amazon".
        for marker in [" on ", " in "] {
            if let Some((query, place)) = rsplit_ci(query, marker)
                && let Some(found) = engine(place)
            {
                return search_action(query, found);
            }
        }
        // "find" and "look" alone say nothing about the web: "find my keys".
        let web_verb = lower.starts_with("search") || lower.starts_with("web search")
            || lower.starts_with("look up") || lower.starts_with("look online");
        // "search the logs for errors" names a place Navi does not know.
        if web_verb && (bare.is_some() || !query.to_ascii_lowercase().contains(" for ")) {
            return search_action(query, google);
        }
        return None;
    }
    if let Some(query) = after_word(text, &["google"]) {
        return search_action(strip_ci(query, "for ").unwrap_or(query), google);
    }
    None
}

fn find_site(name: &str) -> Option<&'static Site> {
    let name = name.trim().trim_end_matches("'s").trim().to_ascii_lowercase();
    SITES.iter().find(|site| site.names.contains(&name.as_str()))
}

fn page_of(word: &str) -> Option<&'static str> {
    match word.trim().trim_end_matches('.').to_ascii_lowercase().as_str() {
        "notifications" | "notification" | "alerts" | "mentions" | "activity" => Some("notifications"),
        "messages" | "message" | "dms" | "dm" | "direct messages" | "inbox" | "chats" => Some("messages"),
        _ => None,
    }
}

fn has_page(site: &Site, page: &str) -> bool {
    if page == "notifications" { site.notifications.is_some() } else { site.messages.is_some() }
}

fn open_site(site: &Site, page: Option<&str>) -> Value {
    let (url, say) = match page {
        Some("notifications") => (site.notifications.unwrap_or(site.home), format!("Opening your {} notifications.", site.label)),
        Some("messages") => (site.messages.unwrap_or(site.home), format!("Opening your {} messages.", site.label)),
        _ => (site.home, format!("Opening {}.", site.label)),
    };
    json!({"kind":"desk","op":"web","url":url,"say":say})
}

/// "check my x notifications", "open my linkedin messages", "show my instagram
/// dms", "check reddit". The browser's own login is what gets the user in.
fn parse_social(text: &str) -> Option<Value> {
    let rest = after_word(text, &["check", "open", "show me", "show", "go to", "take me to", "pull up", "bring up", "read", "see"])?;
    let checking = strip_ci(text, "check ").is_some();
    let rest = after_word(rest, &["my", "the"]).unwrap_or(rest);
    // "notifications on x", "messages on linkedin".
    if let Some((page, place)) = rsplit_ci(rest, " on ")
        && let (Some(page), Some(site)) = (page_of(after_word(page, &["my", "the"]).unwrap_or(page)), find_site(place))
        && has_page(site, page)
    {
        return Some(open_site(site, Some(page)));
    }
    // "x notifications", "linkedin direct messages".
    if let Some((name, page)) = rest.split_once(' ') {
        if let (Some(site), Some(page)) = (find_site(name), page_of(page))
            && has_page(site, page)
        {
            return Some(open_site(site, Some(page)));
        }
        if let Some((name, page)) = rest.rsplit_once(' ')
            && let (Some(site), Some(page)) = (find_site(name), page_of(page))
            && has_page(site, page)
        {
            return Some(open_site(site, Some(page)));
        }
    }
    // "check my instagram": "open" alone waits for the late phase, where an
    // installed app or a session of that name can win.
    if checking {
        return find_site(rest).map(|site| open_site(site, None));
    }
    None
}

/// "post hello on x", "tweet hello", "post on linkedin saying hello". This only
/// fills the site's compose box: the user presses Post themselves.
fn parse_post(text: &str) -> Option<Value> {
    let (message, place) = if let Some(rest) = after_word(text, &["tweet"]) {
        (rest.to_string(), "x".to_string())
    } else {
        let rest = after_word(text, &["post", "share", "publish"])?;
        if let Some(rest) = after_word(rest, &["on", "to"]) {
            let (place, message) = split_ci(rest, " saying ").or_else(|| split_ci(rest, ": "))?;
            (message.to_string(), place.to_string())
        } else {
            let (message, place) = rsplit_ci(rest, " on ").or_else(|| rsplit_ci(rest, " to "))?;
            (message.to_string(), place.to_string())
        }
    };
    let site = find_site(&place)?;
    if site.compose.is_none() && !matches!(site.label, "Facebook" | "Instagram" | "TikTok") {
        return None;
    }
    let message = message.trim().trim_matches(['"', '“', '”']).trim();
    if message.is_empty() {
        return None;
    }
    let url = match site.compose {
        Some(compose) => compose.replace("{}", &url_encode(message)),
        None => site.home.to_string(),
    };
    let say = if site.compose.is_some() {
        format!("Your post is ready on {}. Nothing goes out until you press Post.", site.label)
    } else {
        format!("{} has no way to pre-fill a post, so I opened it for you to paste it.", site.label)
    };
    Some(json!({"kind":"desk","op":"web","url":url,"say":say}))
}

/// "bob at example dot com" from speech becomes an address.
fn spoken_address(who: &str) -> String {
    let lower = who.trim().to_ascii_lowercase();
    if lower.contains('@') {
        return lower.replace(' ', "");
    }
    let words: Vec<&str> = lower.split_whitespace().collect();
    if words.contains(&"at") && words.contains(&"dot") {
        return lower.replace(" at ", "@").replace(" dot ", ".").replace(' ', "");
    }
    who.trim().to_string()
}

fn valid_address(address: &str) -> bool {
    let Some((name, domain)) = address.split_once('@') else {
        return false;
    };
    !name.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && address.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | '+'))
}

/// A sentence that asks about mail, in the many ways people ask it.
fn parse_mail(text: &str) -> Option<Value> {
    let lower = text.to_ascii_lowercase();
    if let Some(send) = parse_mail_send(text) {
        return Some(send);
    }
    let about_mail = ["email", "emails", "e-mail", "e-mails", "mail", "inbox"]
        .iter()
        .any(|word| lower.split(|c: char| !c.is_alphanumeric() && c != '-').any(|token| token == *word));
    const WORK: &[&str] = &["server", "code", "bug", "function", "test", "tests", "api", "endpoint", "script", "feature",
        "validation", "log", "logs", "template", "service", "module", "class", "field", "regex"];
    if !about_mail || lower.split(|c: char| !c.is_alphanumeric()).any(|token| WORK.contains(&token)) {
        return None;
    }
    if ["open ", "go to ", "take me to ", "pull up ", "bring up "].iter().any(|lead| lower.starts_with(lead)) {
        let rest = lower.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
        let rest = rest.strip_prefix("me to ").unwrap_or(&rest).strip_prefix("to ").unwrap_or(&rest);
        let rest = rest.strip_prefix("my ").or_else(|| rest.strip_prefix("the ")).unwrap_or(rest);
        return matches!(rest, "email" | "emails" | "e-mail" | "mail" | "inbox" | "email inbox")
            .then(|| json!({"kind":"desk","op":"mail_open"}));
    }
    let reading = lower.starts_with("read ") || lower.starts_with("what does my ") || lower.starts_with("what's my latest")
        || lower.contains("latest email") || lower.contains("last email") || lower.contains("newest email");
    if reading && (lower.starts_with("read ") || lower.contains("email")) {
        return Some(json!({"kind":"desk","op":"mail_read"}));
    }
    const ASKS: &[&str] = &["check my", "check the", "check email", "check mail", "check for", "any new", "any unread", "do i have",
        "did i get", "have i got", "how many", "what's in my", "whats in my", "what is in my", "who emailed", "show my",
        "show me my", "list my", "my unread"];
    ASKS.iter().any(|ask| lower.starts_with(ask)).then(|| json!({"kind":"desk","op":"mail_check"}))
}

fn parse_mail_send(text: &str) -> Option<Value> {
    let rest = ["send an email to ", "send email to ", "send a mail to ", "send an e-mail to ", "write an email to ", "email "]
        .iter()
        .find_map(|lead| strip_ci(text, lead))?;
    let (head, body) = [" saying ", " that says ", " telling them ", ", saying ", ": "]
        .iter()
        .find_map(|marker| split_ci(rest, marker))
        .unwrap_or((rest, ""));
    let (who, subject) = split_ci(head, " with the subject ")
        .or_else(|| split_ci(head, " with subject "))
        .or_else(|| split_ci(head, " about "))
        .unwrap_or((head, ""));
    let to = spoken_address(who);
    // "email validation is broken" is a remark, not a command: without a message
    // it takes a real address to be one. A name is at most a few words.
    if to.is_empty() || to.split_whitespace().count() > 3 || (body.is_empty() && !valid_address(&to)) {
        return None;
    }
    let body = body.trim().trim_matches(['"', '“', '”']).trim();
    let subject = if subject.is_empty() {
        body.split_whitespace().take(8).collect::<Vec<_>>().join(" ")
    } else {
        subject.trim_matches(['"', '“', '”']).to_string()
    };
    Some(json!({"kind":"desk","op":"mail_send","to":to,"subject":subject,"body":body}))
}

fn file_kind(word: &str) -> Option<&'static str> {
    match word {
        "pdf" | "pdfs" => Some("pdf"),
        "photo" | "photos" | "picture" | "pictures" | "image" | "images" | "screenshot" | "screenshots" => Some("image"),
        "video" | "videos" | "movie" | "movies" => Some("video"),
        "song" | "songs" | "audio" | "recording" | "recordings" => Some("audio"),
        "spreadsheet" | "spreadsheets" | "excel" => Some("sheet"),
        "presentation" | "presentations" | "slides" => Some("slides"),
        "document" | "documents" | "doc" | "docs" => Some("document"),
        "folder" | "folders" | "directory" => Some("folder"),
        "file" | "files" => Some("any"),
        _ => None,
    }
}

fn kind_extensions(kind: &str) -> &'static [&'static str] {
    match kind {
        "pdf" => &["pdf"],
        "image" => &["png", "jpg", "jpeg", "webp", "gif", "heic", "svg", "bmp"],
        "video" => &["mp4", "mkv", "webm", "mov", "avi"],
        "audio" => &["mp3", "flac", "ogg", "wav", "m4a", "opus"],
        "sheet" => &["xlsx", "xls", "ods", "csv"],
        "slides" => &["pptx", "ppt", "odp", "key"],
        "document" => &["pdf", "docx", "doc", "odt", "txt", "md", "rtf"],
        _ => &[],
    }
}

fn standard_folder(name: &str) -> Option<&'static str> {
    match name {
        "downloads" | "download" => Some("Downloads"),
        "documents" | "docs" => Some("Documents"),
        "pictures" | "photos" => Some("Pictures"),
        "music" => Some("Music"),
        "videos" => Some("Videos"),
        "desktop" => Some("Desktop"),
        "home" => Some(""),
        _ => None,
    }
}

/// "find the file tax report", "where is my resume pdf", "open my downloads".
/// A kind word is required, so "find my keys" stays conversation.
fn parse_file(text: &str) -> Option<Value> {
    let lower = text.to_ascii_lowercase();
    let (open, rest) = if let Some(rest) = after_word(&lower, &["open", "show me", "show"]) {
        (true, rest)
    } else {
        let rest = after_word(&lower, &["find", "locate", "search for", "where is", "where's", "look for"])?;
        (false, rest)
    };
    let words: Vec<&str> = rest
        .split_whitespace()
        .filter(|word| !matches!(*word, "my" | "the" | "a" | "an" | "called" | "named" | "on" | "computer" | "pc" | "machine"))
        .collect();
    if open && words.len() <= 2 {
        let name = words.first().copied().unwrap_or_default();
        let plain = words.len() == 1 || words.get(1).is_some_and(|word| matches!(*word, "folder" | "directory"));
        if plain && let Some(folder) = standard_folder(name) {
            return Some(json!({"kind":"desk","op":"open_folder","folder":folder}));
        }
    }
    let mut kind = None;
    let mut query = Vec::new();
    for word in words {
        match file_kind(word) {
            Some(found) if kind.is_none() => kind = Some(found),
            _ => query.push(word),
        }
    }
    let kind = kind?;
    const SENTENCE: &[&str] = &["in", "that", "which", "where", "with", "for", "why", "how", "bug", "about", "this", "project",
        "structure", "changes", "current"];
    if query.is_empty() || query.len() > 5 || query.iter().any(|word| SENTENCE.contains(word)) {
        return None;
    }
    Some(json!({"kind":"desk","op":"files","open":open,"query":query.join(" "),"type":kind}))
}

fn parse_system(text: &str) -> Option<Value> {
    let lower = text.to_ascii_lowercase();
    let system = |command: &str, value: Value| Some(json!({"kind":"desk","op":"system","command":command,"value":value}));
    match lower.as_str() {
        "volume up" | "turn up the volume" | "turn the volume up" | "raise the volume" | "louder" | "make it louder" => {
            return system("volume_step", json!("10%+"));
        }
        "volume down" | "turn down the volume" | "turn the volume down" | "lower the volume" | "quieter" | "make it quieter" => {
            return system("volume_step", json!("10%-"));
        }
        "mute the sound" | "mute the volume" | "mute the audio" | "mute the speakers" | "mute my computer" | "mute the computer" => {
            return system("mute", json!(true));
        }
        "unmute the sound" | "unmute the volume" | "unmute the audio" | "unmute the speakers" | "unmute my computer" | "unmute the computer" => {
            return system("mute", json!(false));
        }
        "lock the screen" | "lock my screen" | "lock the computer" | "lock my computer" | "lock the pc" | "lock my pc" | "lock screen" => {
            return system("lock", Value::Null);
        }
        "take a screenshot" | "take screenshot" | "screenshot" | "take a screen shot" | "capture the screen" => {
            return system("screenshot", Value::Null);
        }
        "what's my battery" | "whats my battery" | "how much battery do i have" | "battery level" | "how's my battery"
        | "what is my battery" | "battery status" => {
            return system("battery", Value::Null);
        }
        "pause the music" | "pause music" | "pause the song" | "resume the music" | "play the music" | "play music" | "pause playback" => {
            return system("media", json!("play-pause"));
        }
        "next song" | "next track" | "skip this song" | "skip the song" | "skip song" => return system("media", json!("next")),
        "previous song" | "previous track" | "last song" | "go back a song" => return system("media", json!("previous")),
        _ => {}
    }
    let rest = ["set the volume to ", "set volume to ", "volume to ", "volume at ", "turn the volume to "]
        .iter()
        .find_map(|lead| lower.strip_prefix(lead))?;
    let level: u32 = rest.trim().trim_end_matches("percent").trim().trim_end_matches('%').trim().parse().ok()?;
    (level <= 100).then(|| json!({"kind":"desk","op":"system","command":"volume_set","value":level}))
}

/// "open https://example.com", "go to example.com", "open example dot com".
fn parse_url(text: &str) -> Option<Value> {
    let rest = after_word(text, &["open", "go to", "browse to", "navigate to", "visit", "take me to"])?;
    let candidate = rest.replace(" dot ", ".");
    let candidate = candidate.trim();
    if candidate.contains(' ') || !candidate.contains('.') {
        return None;
    }
    let lower = candidate.to_ascii_lowercase();
    let url = if lower.starts_with("https://") || lower.starts_with("http://") {
        candidate.to_string()
    } else {
        // Only a word that ends in a plausible domain: "readme.md" is a file.
        let host = lower.split('/').next().unwrap_or_default();
        let tld = host.rsplit('.').next().unwrap_or_default();
        const FILE_TLDS: &[&str] = &["md", "rs", "py", "txt", "pdf", "png", "jpg", "zip", "sh", "pl", "json", "toml", "lock"];
        if tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_alphabetic()) || FILE_TLDS.contains(&tld) {
            return None;
        }
        format!("https://{candidate}")
    };
    let valid = url.chars().all(|c| c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c));
    valid.then(|| json!({"kind":"desk","op":"web","url":url,"say":format!("Opening {candidate}.")}))
}

/// A message's own "and" and "then" are not steps of the command: "email Sara
/// saying see you then" and "post cats and dogs on x" are one action each.
pub fn holds_message(text: &str) -> bool {
    let text = trimmed(text);
    parse_mail_send(&text).is_some() || parse_post(&text).is_some()
}

pub fn early(text: &str) -> Option<Value> {
    let text = trimmed(text);
    if text.is_empty() {
        return None;
    }
    parse_system(&text)
        .or_else(|| parse_url(&text))
        .or_else(|| parse_mail(&text))
        .or_else(|| parse_post(&text))
        .or_else(|| parse_social(&text))
        .or_else(|| parse_file(&text))
        .or_else(|| parse_search(&text))
}

struct DesktopEntry {
    id: String,
    name: String,
}

fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/share/applications"));
        dirs.push(home.join(".local/share/flatpak/exports/share/applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs.push(PathBuf::from("/usr/local/share/applications"));
    dirs.push(PathBuf::from("/usr/share/applications"));
    dirs
}

fn read_entry(path: &Path) -> Option<DesktopEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut section = false;
    let mut name = None;
    for line in text.lines() {
        if line.starts_with('[') {
            // Keys after [Desktop Action …] belong to an action, not the app.
            if section {
                break;
            }
            section = line == "[Desktop Entry]";
            continue;
        }
        if !section {
            continue;
        }
        if let Some(value) = line.strip_prefix("Name=") {
            name = Some(value.trim().to_string());
        }
        if matches!(line, "NoDisplay=true" | "Hidden=true") || line.starts_with("Type=") && line != "Type=Application" {
            return None;
        }
    }
    Some(DesktopEntry { id: path.file_stem()?.to_str()?.to_string(), name: name? })
}

fn desktop_entries() -> Vec<DesktopEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for dir in desktop_dirs() {
        let Ok(files) = std::fs::read_dir(dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|ext| ext == "desktop")
                && let Some(entry) = read_entry(&path)
                && seen.insert(entry.id.clone())
            {
                entries.push(entry);
            }
        }
    }
    entries
}

/// Exact name first; otherwise every entry whose name holds all the words.
fn match_apps(name: &str) -> (Vec<DesktopEntry>, bool) {
    let target = name.trim().to_lowercase();
    let mut exact = Vec::new();
    let mut partial = Vec::new();
    let words: Vec<&str> = target.split_whitespace().collect();
    for entry in desktop_entries() {
        let lower = entry.name.to_lowercase();
        if lower == target {
            exact.push(entry);
        } else if !words.is_empty() && lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).any(|w| words.contains(&w))
            && words.iter().all(|word| lower.split(|c: char| !c.is_alphanumeric()).any(|w| w == *word))
        {
            partial.push(entry);
        }
    }
    if exact.is_empty() { (partial, false) } else { (exact, true) }
}

fn loose_name(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// A folder under the usual roots whose name is what was said, ignoring case
/// and punctuation: "zen relay" finds `~/Documents/zen-relay`. Only a folder
/// that looks like a project counts, so "in Documents" cannot match a random
/// directory.
pub fn discover_project(name: &str) -> Option<PathBuf> {
    let target = loose_name(name);
    if target.len() < 2 {
        return None;
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let roots = std::env::var("NAVI_PROJECT_ROOTS").map_or_else(
        |_| PROJECT_ROOTS.iter().map(|root| home.join(root)).collect::<Vec<_>>(),
        |roots| roots.split(':').filter(|root| !root.is_empty()).map(PathBuf::from).collect(),
    );
    let mut found: Option<PathBuf> = None;
    for root in roots {
        let Ok(children) = std::fs::read_dir(&root) else {
            continue;
        };
        for child in children.flatten() {
            let path = child.path();
            let matches = child.file_name().to_str().is_some_and(|folder| loose_name(folder) == target);
            if matches && path.is_dir() && PROJECT_MARKERS.iter().any(|marker| path.join(marker).exists()) {
                match &found {
                    // Two roots hold that name: which one is the user's call.
                    Some(existing) if existing != &path => return None,
                    _ => found = Some(path),
                }
            }
        }
    }
    found
}

/// What follows "open Codex in …": "zen relay to fix the login bug" → "zen relay".
pub fn project_words(target: &str) -> &str {
    let lower = target.to_ascii_lowercase();
    let cut = [" to ", " with ", " and ", " then ", " called ", " named ", ":", ","]
        .iter()
        .filter_map(|marker| lower.find(marker))
        .min()
        .unwrap_or(target.len());
    target[..cut].trim().trim_end_matches(" project").trim()
}

/// "open spotify", "start firefox", "open reddit", "open the zen-relay project".
/// Nothing is returned for a name that resolves to nothing real, so "open the
/// door" stays conversation.
pub fn late(text: &str) -> Option<Value> {
    let text = trimmed(text);
    let lower = text.to_ascii_lowercase();
    let launching = after_word(&lower, &["open", "launch", "start", "run", "bring up", "pull up", "fire up"]);
    let visiting = after_word(&lower, &["go to", "take me to", "switch to", "visit", "show me"]);
    let target = launching.or(visiting)?;
    let target = after_word(target, &["the", "my"]).unwrap_or(target).trim();
    let named = target.strip_suffix(" app").or_else(|| target.strip_suffix(" application")).unwrap_or(target).trim();
    if named.is_empty() {
        return None;
    }
    if matches!(named, "browser" | "web browser" | "internet" | "zen" | "zen browser") {
        return Some(json!({"kind":"desk","op":"browser"}));
    }
    if launching.is_some() {
        let (apps, exact) = match_apps(named);
        if exact && let Some(app) = apps.first() {
            return Some(json!({"kind":"desk","op":"app","ids":[app.id],"name":app.name}));
        }
        // "Run the build" and "start the server" are work, not an app that has
        // one of those words in its name.
        let loose_verb = after_word(&lower, &["open", "launch", "bring up", "pull up", "fire up"]).is_some();
        if loose_verb && find_site(named).is_none() && !apps.is_empty() {
            let candidates: Vec<Value> = apps.iter().take(4).map(|app| json!({"id":app.id,"label":app.name})).collect();
            let name = apps[0].name.clone();
            return Some(json!({"kind":"desk","op":"app","candidates":candidates,"name":name}));
        }
    }
    if let Some(site) = find_site(named) {
        return Some(open_site(site, None));
    }
    let project = project_words(named);
    discover_project(project).map(|path| {
        json!({"kind":"desk","op":"project","path":path,"name":path.file_name().and_then(|name| name.to_str()).unwrap_or(project)})
    })
}

fn detached(command: &mut Command) -> Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    // Reaped on its own thread, so a long-lived daemon keeps no zombies.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn tool_on_path(name: &str) -> Option<String> {
    adapters::resolve(name, &adapters::launch_path_dirs()).ok()
}

fn wanted_extension(path: &Path, kind: &str) -> bool {
    match kind {
        "any" => true,
        "folder" => path.is_dir(),
        _ => path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| kind_extensions(kind).contains(&ext.to_ascii_lowercase().as_str())),
    }
}

/// Dot-folders and dependency folders are not "my files": a search for "report"
/// should not answer with a copy inside a package cache.
fn visible_under(path: &Path, home: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(home) else {
        return false;
    };
    relative.components().all(|part| {
        part.as_os_str().to_str().is_some_and(|name| !name.starts_with('.') && !SKIP_DIRS.contains(&name))
    })
}

fn locate_candidates(words: &[&str], home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(plocate) = tool_on_path("plocate") {
        let mut command = Command::new(plocate);
        command.args(["-i", "-b", "-A", "-l", "2000", "--"]).args(words);
        if let Ok(output) = adapters::run_timeout(&mut command, Duration::from_secs(5))
            && output.status.success()
        {
            paths.extend(String::from_utf8_lossy(&output.stdout).lines().map(PathBuf::from));
        }
    }
    if !paths.is_empty() {
        return paths;
    }
    // No index, or it is stale: a bounded walk of the folders people keep things in.
    let mut stack: Vec<(PathBuf, usize)> = ["Documents", "Downloads", "Desktop", "Pictures", "Music", "Videos"]
        .iter()
        .map(|folder| (home.join(folder), 0))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut visited = 0;
    while let Some((dir, depth)) = stack.pop() {
        if visited > 40_000 || Instant::now() > deadline {
            break;
        }
        let Ok(children) = std::fs::read_dir(&dir) else {
            continue;
        };
        for child in children.flatten() {
            visited += 1;
            let path = child.path();
            let name = child.file_name().to_string_lossy().to_lowercase();
            if words.iter().all(|word| name.contains(*word)) {
                paths.push(path.clone());
            }
            if depth < 6
                && child.file_type().is_ok_and(|kind| kind.is_dir())
                && !name.starts_with('.')
                && !SKIP_DIRS.contains(&name.as_str())
            {
                stack.push((path, depth + 1));
            }
        }
    }
    paths
}

/// Newest first, at most `limit`, only existing files of the wanted kind.
fn find_files(query: &str, kind: &str, limit: usize) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut hits: Vec<(std::time::SystemTime, PathBuf)> = locate_candidates(&words, &home)
        .into_iter()
        .filter(|path| visible_under(path, &home) && wanted_extension(path, kind))
        .filter_map(|path| Some((std::fs::metadata(&path).ok()?.modified().ok()?, path)))
        .collect();
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.0));
    hits.dedup_by(|a, b| a.1 == b.1);
    hits.into_iter().take(limit).map(|hit| hit.1).collect()
}

fn pretty_path(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from).and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// The reply to one desk action. `pending` is a follow-up question: the next
/// command answers it, through the same mechanism as "Which one?".
#[derive(Default)]
pub struct Outcome {
    pub text: String,
    pub speech: Option<String>,
    pub pending: Option<Value>,
}

fn says(text: impl Into<String>) -> Outcome {
    Outcome { text: text.into(), ..Outcome::default() }
}

impl App {
    /// The user's browser: `NAVI_BROWSER`, else Zen, else the desktop default.
    fn open_in_browser(&self, url: Option<&str>) -> Result<()> {
        if let Some(url) = url
            && !url.starts_with("https://")
            && !url.starts_with("http://")
        {
            return Err("Only http and https links can be opened".into());
        }
        if let Ok(browser) = std::env::var("NAVI_BROWSER") {
            let mut command = Command::new(browser);
            if let Some(url) = url {
                command.arg(url);
            }
            return detached(&mut command);
        }
        if self.isolated {
            return Err("Opening the user's browser is disabled in isolated mode".into());
        }
        if let Some(zen) = tool_on_path("zen") {
            let mut command = Command::new(zen);
            if let Some(url) = url {
                command.args(["--new-tab", url]);
            }
            return detached(&mut command);
        }
        let target = url.unwrap_or("https://www.google.com/");
        detached(Command::new("xdg-open").arg(target))
    }

    pub(crate) fn open_url(&self, url: &str) -> Result<()> {
        self.open_in_browser(Some(url))
    }

    pub(crate) fn open_browser(&self) -> Result<()> {
        self.open_in_browser(None)
    }

    fn open_path(&self, path: &Path) -> Result<()> {
        if self.isolated {
            return Err("Opening files is disabled in isolated mode".into());
        }
        detached(Command::new("xdg-open").arg(path))
    }

    /// One call to the stdlib-only mail worker. Params, which may hold a
    /// password, travel on stdin and never on the command line.
    pub(crate) fn mail_op(&self, op: &str, params: &Value) -> Result<Value> {
        let mut child = Command::new(self.python())
            .arg(self.root.join("workers/mail.py"))
            .arg(op)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(params.to_string().as_bytes()).map_err(|error| error.to_string())?;
        }
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            match child.try_wait().map_err(|error| error.to_string())? {
                Some(_) => break,
                None if Instant::now() > deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("The mail server did not answer in time".into());
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let output = child.wait_with_output().map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| "The mail worker returned no data".to_string())?;
        if value["ok"] == false {
            return Err(value["error"].as_str().unwrap_or("Mail failed").to_string());
        }
        Ok(value)
    }

    fn webmail(&self) -> &'static Site {
        let address = self.mail_op("status", &json!({})).ok();
        let domain = address
            .as_ref()
            .and_then(|status| status["address"].as_str())
            .and_then(|address| address.split_once('@'))
            .map_or(String::new(), |(_, domain)| domain.to_ascii_lowercase());
        let name = match domain.as_str() {
            "outlook.com" | "hotmail.com" | "live.com" | "msn.com" => "outlook",
            "yahoo.com" => "yahoo mail",
            "proton.me" | "protonmail.com" | "pm.me" => "proton mail",
            _ => "gmail",
        };
        find_site(name).unwrap_or(&SITES[0])
    }

    fn mail_reply(&self, op: &str) -> Result<Outcome> {
        let status = self.mail_op("status", &json!({}))?;
        if status["connected"] != true {
            let site = self.webmail();
            self.open_url(site.home)?;
            return Ok(says(format!(
                "Your email is not connected yet, so I opened {}. To have me read it to you, add an app password in Settings under Connections.",
                site.label
            )));
        }
        let value = self.mail_op(op, &json!({}))?;
        Ok(Outcome {
            text: value["text"].as_str().unwrap_or("Done.").to_string(),
            speech: value["speech"].as_str().map(str::to_string),
            ..Outcome::default()
        })
    }

    fn mail_send(&self, action: &Value) -> Result<Outcome> {
        let to = action["to"].as_str().unwrap_or_default();
        let subject = action["subject"].as_str().unwrap_or_default();
        let body = action["body"].as_str().unwrap_or_default();
        let connected = self.mail_op("status", &json!({}))?["connected"] == true;
        if !connected {
            if !valid_address(to) {
                return Err(format!(
                    "I can only find “{to}” once your email is connected. Connect it in Settings, or say the full address."
                ));
            }
            let url = format!(
                "https://mail.google.com/mail/?view=cm&to={}&su={}&body={}",
                url_encode(to), url_encode(subject), url_encode(body)
            );
            self.open_url(&url)?;
            return Ok(says(format!("Your email to {to} is drafted in the browser. Nothing goes out until you press Send.")));
        }
        if action["confirmed"] == true {
            self.mail_op("send", &json!({"to":to,"subject":subject,"body":body}))?;
            return Ok(says(format!("Sent your email to {to}.")));
        }
        // A name is looked up in who has written to the user; the address it
        // resolves to is shown in the question, so a wrong match is caught.
        let address = if valid_address(to) {
            to.to_string()
        } else {
            let found = self.mail_op("contact", &json!({"name":to}))?;
            found["address"].as_str().ok_or_else(|| {
                found["error"].as_str().map_or_else(|| format!("I have no address for {to}."), str::to_string)
            })?.to_string()
        };
        if body.is_empty() {
            return Err("What should the email say? Say “email … saying …”.".into());
        }
        let mut confirm = action.clone();
        confirm["to"] = json!(address);
        confirm["confirmed"] = json!(true);
        Ok(Outcome {
            text: format!("Send this to {address}: “{body}”? Say yes to send."),
            pending: Some(json!({"time":now(),"action":confirm,"confirm":true,"decline":"Okay, nothing was sent.",
                "choices":[{"id":"yes","label":"Yes"},{"id":"no","label":"No"}]})),
            ..Outcome::default()
        })
    }

    fn run_system(&self, action: &Value) -> Result<Outcome> {
        if self.isolated {
            return Err("System controls are disabled in isolated mode".into());
        }
        let run = |program: &str, args: &[&str]| -> Result<String> {
            let path = tool_on_path(program).ok_or_else(|| format!("{program} is not installed"))?;
            let mut command = Command::new(path);
            command.args(args);
            let output = adapters::run_timeout(&mut command, Duration::from_secs(10)).map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(format!("{program} failed"));
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        };
        let sink = "@DEFAULT_AUDIO_SINK@";
        let level = || -> String {
            run("wpctl", &["get-volume", sink])
                .ok()
                .and_then(|out| out.split_whitespace().nth(1).and_then(|value| value.parse::<f64>().ok()))
                .map_or("changed".to_string(), |value| format!("at {:.0} percent", value * 100.0))
        };
        match action["command"].as_str().unwrap_or_default() {
            "volume_step" => {
                run("wpctl", &["set-volume", "-l", "1.0", sink, action["value"].as_str().unwrap_or("5%+")])?;
                Ok(says(format!("Volume {}.", level())))
            }
            "volume_set" => {
                let percent = action["value"].as_u64().unwrap_or(50).min(100);
                run("wpctl", &["set-volume", sink, &format!("{percent}%")])?;
                Ok(says(format!("Volume at {percent} percent.")))
            }
            "mute" => {
                let on = action["value"] == true;
                run("wpctl", &["set-mute", sink, if on { "1" } else { "0" }])?;
                Ok(says(if on { "Sound muted." } else { "Sound on." }))
            }
            "lock" => {
                run("loginctl", &["lock-session"])?;
                Ok(says("Screen locked."))
            }
            "screenshot" => {
                let home = PathBuf::from(std::env::var_os("HOME").ok_or("No home directory")?);
                let folder = home.join("Pictures/Screenshots");
                std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
                let path = folder.join(format!("navi-{}.png", now()));
                let target = path.to_string_lossy().into_owned();
                run("spectacle", &["-b", "-n", "-f", "-o", &target])?;
                Ok(Outcome {
                    text: format!("Saved a screenshot to {}.", pretty_path(&path)),
                    speech: Some("Screenshot saved.".into()),
                    ..Outcome::default()
                })
            }
            "battery" => {
                let levels: Vec<u32> = std::fs::read_dir("/sys/class/power_supply")
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("BAT"))
                    .filter_map(|entry| std::fs::read_to_string(entry.path().join("capacity")).ok()?.trim().parse().ok())
                    .collect();
                Ok(says(match levels.first() {
                    Some(percent) => format!("Battery at {percent} percent."),
                    None => "This computer has no battery.".to_string(),
                }))
            }
            "media" => {
                if tool_on_path("playerctl").is_none() {
                    return Err("Controlling music needs playerctl. Install it with: sudo dnf install playerctl".into());
                }
                run("playerctl", &[action["value"].as_str().unwrap_or("play-pause")])?;
                Ok(says("Okay."))
            }
            _ => Err("Unknown system command".into()),
        }
    }

    fn run_files(&self, action: &Value) -> Result<Outcome> {
        // The answer to "Which one?": the path was one this search returned.
        if let Some(path) = action["ids"].as_array().and_then(|ids| ids.first()).and_then(Value::as_str) {
            let path = Path::new(path);
            self.open_path(path)?;
            return Ok(says(format!("Opened {}.", file_name(path))));
        }
        let query = action["query"].as_str().unwrap_or_default();
        let kind = action["type"].as_str().unwrap_or("any");
        let found = find_files(query, kind, 5);
        if found.is_empty() {
            return Ok(says(format!("I could not find a file matching “{query}” in your folders.")));
        }
        if action["open"] == true && found.len() == 1 {
            self.open_path(&found[0])?;
            return Ok(says(format!("Opened {}.", file_name(&found[0]))));
        }
        let list = found.iter().map(|path| pretty_path(path)).collect::<Vec<_>>().join("\n");
        let names = found.iter().map(|path| file_name(path)).collect::<Vec<_>>().join(", ");
        let mut opener = action.clone();
        opener["open"] = json!(true);
        let choices: Vec<Value> = found.iter().map(|path| json!({"id":path,"label":file_name(path)})).collect();
        let count = found.len();
        let lead = if action["open"] == true { "Which one" } else { "Found these, newest first. Open which one" };
        Ok(Outcome {
            text: format!("{lead}?\n{list}"),
            speech: Some(match count {
                1 => format!("I found {names}. Should I open it?"),
                _ => format!("I found {count} files. {names}. Which one?"),
            }),
            pending: Some(json!({"time":now(),"action":opener,"choices":choices})),
        })
    }

    pub(crate) fn run_desk(&self, action: &Value) -> Result<Outcome> {
        match action["op"].as_str().unwrap_or_default() {
            "web" => {
                self.open_url(action["url"].as_str().ok_or("Missing link")?)?;
                Ok(says(action["say"].as_str().unwrap_or("Opened it in your browser.")))
            }
            "browser" => {
                self.open_browser()?;
                Ok(says("Opened your browser."))
            }
            "app" => {
                let id = action["ids"].as_array().and_then(|ids| ids.first()).and_then(Value::as_str);
                let Some(id) = id else {
                    let candidates = action["candidates"].as_array().cloned().unwrap_or_default();
                    let names = candidates.iter().filter_map(|c| c["label"].as_str()).collect::<Vec<_>>();
                    if let [_] = names.as_slice() {
                        let mut single = action.clone();
                        single["ids"] = json!([candidates[0]["id"]]);
                        return self.run_desk(&single);
                    }
                    return Ok(Outcome {
                        text: format!("Which one: {}?", names.join(", ")),
                        pending: Some(json!({"time":now(),"action":action,"choices":candidates})),
                        ..Outcome::default()
                    });
                };
                if self.isolated {
                    return Err("Opening desktop applications is disabled in isolated mode".into());
                }
                let launcher = tool_on_path("gtk-launch").ok_or("gtk-launch is not installed")?;
                let mut command = Command::new(launcher);
                command.arg(id);
                let output = adapters::run_timeout(&mut command, Duration::from_secs(10)).map_err(|error| error.to_string())?;
                if !output.status.success() {
                    return Err(format!("{} did not start", action["name"].as_str().unwrap_or(id)));
                }
                // "Which one?" names the choice in `ids`; the label only names the first.
                let shown = action["candidates"]
                    .as_array()
                    .and_then(|list| list.iter().find(|choice| choice["id"] == id))
                    .and_then(|choice| choice["label"].as_str())
                    .or(action["name"].as_str())
                    .unwrap_or("it");
                Ok(says(format!("Opened {shown}.")))
            }
            "open_folder" => {
                let home = PathBuf::from(std::env::var_os("HOME").ok_or("No home directory")?);
                let folder = action["folder"].as_str().unwrap_or_default();
                self.open_path(&home.join(folder))?;
                Ok(says(if folder.is_empty() { "Opened your home folder.".to_string() } else { format!("Opened {folder}.") }))
            }
            "files" => self.run_files(action),
            "system" => self.run_system(action),
            "mail_open" => {
                let site = self.webmail();
                self.open_url(site.home)?;
                Ok(says(format!("Opening {}.", site.label)))
            }
            "mail_check" => self.mail_reply("check"),
            "mail_read" => self.mail_reply("read"),
            "mail_send" => self.mail_send(action),
            _ => Err("Unknown desk action".into()),
        }
    }

    /// The registered project at this folder, adding it on first use.
    pub(crate) fn register_discovered(&self, path: &Path) -> Result<Value> {
        let projects = self.projects()?;
        let text = path.to_string_lossy();
        if let Some(project) = projects.iter().find(|project| project["path"].as_str() == Some(&text)) {
            return Ok(project.clone());
        }
        let name = path.file_name().and_then(|name| name.to_str()).ok_or("That folder has no name")?;
        self.add_project(name, &text)
    }

    /// How `navid desk` reaches this file: everything Navi can do directly, for
    /// the assistant's shell tool, and never a model turn, so it cannot recurse.
    /// Sending mail is refused here. Text the assistant read in a web page or an
    /// email must not be able to send anything, so that takes the user's own
    /// command and their spoken or typed yes.
    pub(crate) fn desk_command(&self, text: &str, params: &Value) -> Result<Value> {
        let Some(action) = self.direct_intent(text)? else {
            return Err(format!("Navi has no direct command for “{text}”. Do it with your own tools instead."));
        };
        if action["op"] == "mail_send" {
            return Err("Sending email needs the user's own command. Ask them to say: email <who> saying <message>.".into());
        }
        self.run_direct(text, &action, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url_of(action: Option<Value>) -> String {
        action.expect("parsed")["url"].as_str().expect("url").to_string()
    }

    #[test]
    fn web_searches_are_read_in_the_ways_people_say_them() {
        for (said, expected) in [
            ("search for flights to Cairo", "https://www.google.com/search?q=flights%20to%20Cairo"),
            ("Search the web for flights to Cairo.", "https://www.google.com/search?q=flights%20to%20Cairo"),
            ("can you google the weather in Alexandria", "https://www.google.com/search?q=the%20weather%20in%20Alexandria"),
            ("hey Navi, look up rust traits for me", "https://www.google.com/search?q=rust%20traits"),
            ("search youtube for lofi", "https://www.youtube.com/results?search_query=lofi"),
            ("search for lofi on youtube", "https://www.youtube.com/results?search_query=lofi"),
            ("play jazz on youtube", "https://www.youtube.com/results?search_query=jazz"),
            ("find a cheap keyboard on amazon", "https://www.amazon.com/s?k=a%20cheap%20keyboard"),
            ("look up tokio on github", "https://github.com/search?q=tokio"),
        ] {
            assert_eq!(url_of(early(said)), expected, "{said}");
        }
    }

    #[test]
    fn local_work_and_questions_are_not_web_searches() {
        for said in [
            "search the project for TODO",
            "search the logs for errors",
            "search for TODO in the code",
            "find my keys",
            "what is rust",
            "how do I search for files",
            "tell Codex to search for the bug",
            "open Codex to fix the login bug",
            "explain how google works",
        ] {
            assert!(early(said).is_none(), "{said} -> {:?}", early(said));
        }
    }

    #[test]
    fn links_are_opened_only_as_web_addresses() {
        assert_eq!(url_of(early("open https://example.com/a?b=1")), "https://example.com/a?b=1");
        assert_eq!(url_of(early("go to example.com")), "https://example.com");
        assert_eq!(url_of(early("open example dot com")), "https://example.com");
        for said in ["open readme.md", "open file:///etc/passwd", "open javascript:alert(1)", "open main.rs"] {
            assert!(parse_url(said).is_none(), "{said}");
        }
    }

    #[test]
    fn social_pages_open_by_name() {
        assert_eq!(url_of(early("check my x notifications")), "https://x.com/notifications");
        assert_eq!(url_of(early("open my linkedin messages")), "https://www.linkedin.com/messaging/");
        assert_eq!(url_of(early("show my instagram dms")), "https://www.instagram.com/direct/inbox/");
        assert_eq!(url_of(early("check my messages on reddit")), "https://www.reddit.com/message/inbox");
        assert_eq!(url_of(early("check facebook")), "https://www.facebook.com/");
    }

    #[test]
    fn a_post_only_fills_the_compose_box() {
        let post = early("tweet hello world").expect("parsed");
        assert_eq!(post["url"], "https://x.com/intent/post?text=hello%20world");
        assert!(post["say"].as_str().unwrap().contains("until you press Post"));
        assert_eq!(
            url_of(early("post on linkedin saying we shipped")),
            "https://www.linkedin.com/feed/?shareActive=true&text=we%20shipped"
        );
        assert_eq!(url_of(early("post good morning on bluesky")), "https://bsky.app/intent/compose?text=good%20morning");
    }

    #[test]
    fn mail_questions_and_sending_are_told_apart() {
        for said in ["check my email", "any new emails", "do I have any unread email", "what's in my inbox", "how many unread emails"] {
            assert_eq!(early(said).expect(said)["op"], "mail_check", "{said}");
        }
        for said in ["read my latest email", "read my email", "what does my last email say"] {
            assert_eq!(early(said).expect(said)["op"], "mail_read", "{said}");
        }
        assert_eq!(early("open my email").expect("open")["op"], "mail_open");
        let send = early("send an email to bob at example dot com saying the build is fixed").expect("send");
        assert_eq!(send["op"], "mail_send");
        assert_eq!(send["to"], "bob@example.com");
        assert_eq!(send["body"], "the build is fixed");
        let named = early("email Sara about lunch saying see you at one").expect("named");
        assert_eq!(named["to"], "Sara");
        assert_eq!(named["subject"], "lunch");
        assert_eq!(named["body"], "see you at one");
        for said in ["what is an email", "the email server is down", "how does email work"] {
            assert!(early(said).is_none(), "{said}");
        }
    }

    #[test]
    fn files_need_a_kind_word_and_folders_a_known_name() {
        let found = early("find the file tax report").expect("find");
        assert_eq!((found["op"].as_str(), found["query"].as_str(), found["type"].as_str()), (Some("files"), Some("tax report"), Some("any")));
        let pdf = early("where is my resume pdf").expect("pdf");
        assert_eq!((pdf["query"].as_str(), pdf["type"].as_str()), (Some("resume"), Some("pdf")));
        let opening = early("open my invoice spreadsheet").expect("open");
        assert_eq!((opening["open"].as_bool(), opening["type"].as_str()), (Some(true), Some("sheet")));
        assert_eq!(early("open my downloads").expect("folder")["folder"], "Downloads");
        assert_eq!(early("open the documents folder").expect("folder")["folder"], "Documents");
        assert!(early("find my keys").is_none());
    }

    #[test]
    fn system_controls_are_exact_phrases() {
        assert_eq!(early("volume up").expect("up")["command"], "volume_step");
        let set = early("set the volume to 40 percent").expect("set");
        assert_eq!((set["command"].as_str(), set["value"].as_u64()), (Some("volume_set"), Some(40)));
        assert!(early("set the volume to 400").is_none());
        assert_eq!(early("lock the screen").expect("lock")["command"], "lock");
        assert_eq!(early("take a screenshot").expect("shot")["command"], "screenshot");
        assert!(early("mute").is_none(), "plain mute keeps its meaning: Navi's own speech");
    }

    #[test]
    fn work_on_mail_code_is_not_a_question_about_the_inbox() {
        for said in ["check the mail server logs", "check the email validation", "read the email template"] {
            assert!(early(said).is_none(), "{said}");
        }
    }

    #[test]
    fn a_message_keeps_its_own_and_and_then() {
        assert!(holds_message("email bob@example.com saying see you then"));
        assert!(holds_message("post cats and dogs on x"));
        assert!(!holds_message("search for cats then open gmail"));
        assert!(!holds_message("post the results and open gmail"));
    }

    #[test]
    fn urls_escape_everything_a_query_can_hold() {
        assert_eq!(url_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }

    #[test]
    fn courtesy_words_never_touch_the_query() {
        assert_eq!(trimmed("Could you please search for Rust, for me?"), "search for Rust");
        assert_eq!(trimmed("okay navi, let's open Gmail now"), "open Gmail");
    }

    #[test]
    fn project_words_stop_at_the_task() {
        assert_eq!(project_words("zen relay to fix the login bug"), "zen relay");
        assert_eq!(project_words("zen-relay"), "zen-relay");
        assert_eq!(project_words("acme project"), "acme");
    }

    #[test]
    fn a_project_folder_is_found_by_its_loose_name() {
        let root = std::env::temp_dir().join(format!("navi-desk-{}", std::process::id()));
        let project = root.join("zen-relay");
        let plain = root.join("holiday photos");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        // SAFETY: only this test reads this variable.
        unsafe { std::env::set_var("NAVI_PROJECT_ROOTS", &root) };
        assert_eq!(discover_project("Zen Relay").as_deref(), Some(project.as_path()));
        assert_eq!(discover_project("zen-relay").as_deref(), Some(project.as_path()));
        assert_eq!(discover_project("holiday photos"), None, "not a project");
        assert_eq!(discover_project("nothing here"), None);
        unsafe { std::env::remove_var("NAVI_PROJECT_ROOTS") };
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_and_hidden_folders_are_not_my_files() {
        let home = Path::new("/home/u");
        assert!(visible_under(Path::new("/home/u/Documents/report.pdf"), home));
        assert!(!visible_under(Path::new("/home/u/.cache/report.pdf"), home));
        assert!(!visible_under(Path::new("/home/u/app/node_modules/report.pdf"), home));
        assert!(!visible_under(Path::new("/etc/report.pdf"), home));
    }
}
