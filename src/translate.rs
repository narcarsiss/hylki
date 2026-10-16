//! Message translation (#327): the reader sends a message's text to a
//! translation service the user has an account with, under their own key,
//! and shows what comes back in place of the message. Four services:
//!
//! * **DeepL**: the v2 API, free or paid (a free key ends in `:fx` and
//!   goes to `api-free.deepl.com`).
//! * **Google Cloud Translation**: the v2 ("Basic") API with an API key.
//! * **Microsoft Translator**: the v3 API with a resource key, and the
//!   resource's region when it is not a global one.
//! * **LibreTranslate**: any server, self-hosted ones included, with a key
//!   only when the server asks for one.
//!
//! What is sent is the message's text, run by run, with the inline markup
//! inside each run (bold, links) but none of the sender's attributes,
//! stylesheets, images or scripts. Every service is asked to keep the
//! markup, and each run's translation goes back where the run was, so the
//! message keeps its design (#327). A plain-text message goes as its
//! paragraphs and is shown as Reader View shows it.
//!
//! The settings live in `translation.toml`; the key is in the keyring.
//! Which language a message is in, for the optional "Translate" offer, is
//! worked out here, offline: nothing leaves the computer until the user
//! asks for a translation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::i18n::{i18n, i18n_f};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    #[default]
    Off,
    #[serde(rename = "deepl")]
    DeepL,
    Google,
    Microsoft,
    #[serde(rename = "libretranslate")]
    Libre,
}

impl Service {
    /// In the order the Settings picker lists them.
    pub const ALL: [Service; 5] = [Service::Off, Service::DeepL, Service::Google, Service::Microsoft, Service::Libre];

    /// The service's own name, which is not translated.
    pub fn name(self) -> String {
        match self {
            Service::Off => i18n("Off"),
            Service::DeepL => "DeepL".into(),
            Service::Google => "Google Cloud Translation".into(),
            Service::Microsoft => "Microsoft Translator".into(),
            Service::Libre => "LibreTranslate".into(),
        }
    }

    /// The keyring entry its key is kept under.
    fn key_name(self) -> String {
        let id = match self {
            Service::Off => "off",
            Service::DeepL => "deepl",
            Service::Google => "google",
            Service::Microsoft => "microsoft",
            Service::Libre => "libretranslate",
        };
        format!("translate:{id}")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub service: Service,
    /// The language to translate into, as in [`LANGUAGES`]; empty means the
    /// language Hylki is shown in.
    #[serde(default)]
    pub target: String,
    /// Microsoft: the region of the Translator resource ("westeurope");
    /// empty for a global resource.
    #[serde(default)]
    pub region: String,
    /// LibreTranslate: the server, e.g. `https://translate.example.org`.
    #[serde(default)]
    pub url: String,
    /// Offer a translation on messages in another language.
    #[serde(default)]
    pub offer: bool,
}

impl Settings {
    pub fn enabled(&self) -> bool {
        self.service != Service::Off
    }

    /// The language translations go into: the chosen one, else Hylki's own.
    pub fn target_language(&self) -> String {
        if self.target.is_empty() {
            ui_language()
        } else {
            self.target.clone()
        }
    }
}

/// The languages offered as a target, by the code this module uses for
/// them. Every service takes all of these; how each one spells them is
/// [`service_code`]'s business.
pub const LANGUAGES: &[&str] = &[
    "ar", "bg", "cs", "da", "de", "el", "en", "es", "et", "fi", "fr", "he", "hu", "id", "it", "ja", "ko", "lt",
    "lv", "nb", "nl", "pl", "pt-BR", "pt-PT", "ro", "ru", "sk", "sl", "sv", "tr", "uk", "vi", "zh-Hans", "zh-Hant",
];

/// A language as it names itself ("Deutsch"), the way the spell checker's
/// language list shows them.
pub fn language_name(code: &str) -> String {
    match code {
        "ja" => "日本語".into(),
        "zh-Hans" | "zh" => "中文（简体）".into(),
        "zh-Hant" => "中文（繁體）".into(),
        _ => crate::spell::language_display_name(&code.replace('-', "_")),
    }
}

/// The language Hylki's own text is in, as one of [`LANGUAGES`]' codes
/// where it is one: the language chosen in Settings, else the session's.
pub fn ui_language() -> String {
    let chosen = crate::config::load_language();
    let raw = if chosen.is_empty() {
        ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .map(|v| v.split(':').next().unwrap_or_default().to_string())
            .find(|v| !v.is_empty() && v != "C" && v != "POSIX" && !v.starts_with("C."))
            .unwrap_or_else(|| "en".into())
    } else {
        chosen
    };
    normalize(&raw)
}

/// `pt_BR.UTF-8` → `pt-BR`, `zh_TW` → `zh-Hant`, `de_AT` → `de`.
fn normalize(raw: &str) -> String {
    let base = raw.split(['.', '@']).next().unwrap_or(raw).replace('_', "-");
    let lang = base.split('-').next().unwrap_or(&base).to_lowercase();
    let region = base.split('-').nth(1).unwrap_or_default();
    match lang.as_str() {
        "pt" if region.eq_ignore_ascii_case("PT") => "pt-PT".into(),
        "pt" => "pt-BR".into(),
        "zh" if matches!(region, "TW" | "HK" | "MO" | "Hant") => "zh-Hant".into(),
        "zh" => "zh-Hans".into(),
        "no" | "nn" => "nb".into(),
        "iw" => "he".into(),
        "in" => "id".into(),
        _ => lang,
    }
}

/// The language part alone: `pt-BR` → `pt`, `zh-Hant` → `zh`.
pub fn base(code: &str) -> &str {
    code.split('-').next().unwrap_or(code)
}

/// How `service` spells the target language `code`.
fn service_code(service: Service, code: &str) -> String {
    match (service, code) {
        (Service::DeepL, "en") => {
            // DeepL wants the variety; the session's locale says which.
            let british = ["LC_ALL", "LC_MESSAGES", "LANG"]
                .iter()
                .filter_map(|k| std::env::var(k).ok())
                .any(|v| ["en_GB", "en_IE", "en_AU", "en_NZ", "en_ZA", "en_IN"].iter().any(|p| v.starts_with(p)));
            if british { "EN-GB" } else { "EN-US" }.into()
        }
        (Service::DeepL, _) => code.to_uppercase(),
        (Service::Google, "pt-BR") => "pt".into(),
        (Service::Google, "zh-Hans") => "zh-CN".into(),
        (Service::Google, "zh-Hant") => "zh-TW".into(),
        (Service::Google, "nb") => "no".into(),
        (Service::Google, "he") => "iw".into(),
        (Service::Microsoft, "pt-BR") => "pt".into(),
        (Service::Microsoft, "pt-PT") => "pt-pt".into(),
        (Service::Libre, "pt-BR" | "pt-PT") => "pt".into(),
        (Service::Libre, "zh-Hans") => "zh".into(),
        _ => code.into(),
    }
}

/// A language a service reported back, as one of this module's codes.
fn from_service_code(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "zh-tw" | "zh-hant" | "zh-hk" => "zh-Hant".into(),
        l if l.starts_with("zh") => "zh-Hans".into(),
        "pt-pt" => "pt-PT".into(),
        "pt" | "pt-br" => "pt".into(),
        "no" => "nb".into(),
        "iw" => "he".into(),
        _ => base(&lower).to_string(),
    }
}

// ─── Settings file and key ────────────────────────────────────────────────

fn path() -> Option<PathBuf> {
    Some(crate::config::config_base()?.join("hylki").join("translation.toml"))
}

/// Bumped on every save, so an open reader can tell its offer is stale.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

pub fn load() -> Settings {
    let Some(path) = path() else { return Settings::default() };
    let Ok(text) = std::fs::read_to_string(path) else { return Settings::default() };
    toml::from_str(&text).unwrap_or_default()
}

pub fn save(settings: &Settings) {
    let Some(path) = path() else { return };
    match toml::to_string_pretty(settings) {
        Ok(text) => {
            if let Err(e) = crate::config::write_private_file(&path, &text) {
                tracing::warn!("could not save the translation settings: {e}");
            }
        }
        Err(e) => tracing::warn!("could not save the translation settings: {e}"),
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub fn load_key(service: Service) -> Option<String> {
    crate::config::load_cloud_password(&service.key_name()).filter(|k| !k.trim().is_empty())
}

pub fn store_key(service: Service, key: &str) {
    if key.trim().is_empty() {
        crate::config::delete_cloud_password(&service.key_name());
    } else if let Err(e) = crate::config::store_cloud_password(&service.key_name(), key.trim()) {
        tracing::warn!("could not store the {} key: {e}", service.name());
    }
}

// ─── Language detection ───────────────────────────────────────────────────

/// Which language a message's text is in, when that can be told with
/// confidence: one of this module's codes, or `None`. Runs offline.
pub fn detect(html: &str) -> Option<String> {
    let text = visible_text(html);
    // A line or two is too little to go on, and a long newsletter is
    // decided well before its end.
    let text: String = text.chars().take(4000).collect();
    if text.chars().filter(|c| c.is_alphabetic()).count() < 40 {
        return None;
    }
    let info = whatlang::detect(&text)?;
    if !info.is_reliable() {
        return None;
    }
    let code = match info.lang().code() {
        "ara" => "ar",
        "bul" => "bg",
        "cat" => "ca",
        "ces" => "cs",
        "cmn" => "zh-Hans",
        "dan" => "da",
        "deu" => "de",
        "ell" => "el",
        "eng" => "en",
        "est" => "et",
        "fin" => "fi",
        "fra" => "fr",
        "heb" => "he",
        "hin" => "hi",
        "hrv" => "hr",
        "hun" => "hu",
        "ind" => "id",
        "ita" => "it",
        "jpn" => "ja",
        "kor" => "ko",
        "lav" => "lv",
        "lit" => "lt",
        "nld" => "nl",
        "nob" => "nb",
        "pol" => "pl",
        "por" => "pt",
        "ron" => "ro",
        "rus" => "ru",
        "slk" => "sk",
        "slv" => "sl",
        "spa" => "es",
        "srp" => "sr",
        "swe" => "sv",
        "tha" => "th",
        "tur" => "tr",
        "ukr" => "uk",
        "vie" => "vi",
        other => other,
    };
    Some(code.to_string())
}

/// Whether a message in `detected` is worth offering to translate into
/// `target`: a different language, not a variety of the same one.
pub fn differs(detected: &str, target: &str) -> bool {
    base(detected) != base(target)
}

/// The words of a fragment of HTML, roughly: tags dropped, the common
/// entities decoded, quoted text left out. Only good enough for telling
/// the language, which is all it is used for.
fn visible_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut tag = String::new();
    let mut skip_depth = 0usize;
    for c in html.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                let t = tag.trim().to_ascii_lowercase();
                let name: String = t.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                if matches!(name.as_str(), "blockquote" | "style" | "script") {
                    if t.starts_with('/') {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else {
                        skip_depth += 1;
                    }
                }
                out.push(' ');
                tag.clear();
            } else {
                tag.push(c);
            }
        } else if c == '<' {
            in_tag = true;
        } else if skip_depth == 0 {
            out.push(c);
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    // Plain-text replies quote with "> " lines.
    out.lines().filter(|l| !l.trim_start().starts_with('>')).collect::<Vec<_>>().join("\n")
}

// ─── Translating ──────────────────────────────────────────────────────────

/// A translated message.
#[derive(Clone, Debug, PartialEq)]
pub struct Translated {
    /// The translation: the whole message with its text replaced, or, for
    /// plain text, a Reader View fragment.
    pub html: String,
    /// `html` is a Reader View fragment, to be shown as Reader View shows
    /// a message.
    pub reader: bool,
    /// The language the service found the message to be in.
    pub from: Option<String>,
    pub service: Service,
    pub to: String,
    /// The subject, translated with the body in the same request.
    pub subject: Option<String>,
}

/// The most of a message, in characters of HTML, that is
/// sent in one piece: under every service's per-text limit, and a size
/// whose loss to an error costs little of the user's allowance.
const CHUNK: usize = 4_500;
/// How much goes in one request, under the smallest request limit
/// (Microsoft's 50,000 characters).
const BATCH: usize = 40_000;
/// Past this a message is not sent at all: a mailing list digest or a
/// long log would use up a free plan's month in a few clicks.
const LIMIT: usize = 120_000;

/// Translations made this session, so a message opened again is shown
/// translated without asking the service, and paying for it, twice.
static DONE: Mutex<Option<HashMap<String, Translated>>> = Mutex::new(None);

/// What the session cache keys a translation by: the message, the service
/// and the language it went into.
pub fn cache_key(account_id: u32, message: &str, settings: &Settings) -> String {
    format!("{account_id}\u{1f}{message}\u{1f}{:?}\u{1f}{}", settings.service, settings.target_language())
}

pub fn cached(key: &str) -> Option<Translated> {
    DONE.lock().ok()?.as_ref()?.get(key).cloned()
}

/// Translate a message body (as cached: HTML, or plain text) and its
/// subject with the configured service (blocking; call off the UI thread).
pub fn translate(
    settings: &Settings,
    key: &str,
    body: &str,
    subject: &str,
    cache: &str,
) -> Result<Translated, String> {
    if let Some(t) = cached(cache) {
        return Ok(t);
    }
    let service = settings.service;
    if service == Service::Off {
        return Err(i18n("Choose a translation service in Settings → Translation first."));
    }
    if key.is_empty() && service != Service::Libre {
        return Err(i18n_f("No key for {service}: add one in Settings → Translation.", &[("service", &service.name())]));
    }
    let target = settings.target_language();
    let mut document = body.contains('<').then(|| InPlace::parse(body));
    let mut pieces = match &document {
        Some(d) => d.pieces.clone(),
        None => chunk(&prepare_blocks(body)),
    };
    // The subject rides along as one more piece, escaped like the HTML
    // around it, and comes back off the end.
    let subject = subject.trim();
    if !subject.is_empty() {
        pieces.push(gtk::glib::markup_escape_text(subject).to_string());
    }
    let total: usize = pieces.iter().map(String::len).sum();
    if total == 0 {
        return Err(i18n("There is no text to translate."));
    }
    if total > LIMIT {
        return Err(i18n("This message is too long to send for translation."));
    }
    let (mut texts, from) = run(settings, key, &pieces, &target)?;
    let subject = if subject.is_empty() {
        None
    } else {
        texts
            .pop()
            .map(|t| crate::markdown::plain_text(&t).split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|t| !t.is_empty())
    };
    let done = match document.as_mut() {
        Some(d) => Translated { html: d.fill(&texts), reader: false, from, service, to: target, subject },
        None => Translated { html: texts.concat(), reader: true, from, service, to: target, subject },
    };
    if let Ok(mut g) = DONE.lock() {
        let map = g.get_or_insert_with(HashMap::new);
        if map.len() > 200 {
            map.clear();
        }
        map.insert(cache.to_string(), done.clone());
    }
    Ok(done)
}

/// Send `pieces` of HTML to the configured service, to be put into
/// `target`: one translation per piece, in order, and the language the
/// service found the first in.
fn run(settings: &Settings, key: &str, pieces: &[String], target: &str) -> Result<(Vec<String>, Option<String>), String> {
    let service = settings.service;
    let to = service_code(service, target);
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(45)).build();
    let mut out = Vec::with_capacity(pieces.len());
    let mut from = None;
    for batch in batches(pieces, service) {
        let (texts, detected) = match service {
            Service::DeepL => deepl(&agent, key, &batch, &to)?,
            Service::Google => google(&agent, key, &batch, &to)?,
            Service::Microsoft => microsoft(&agent, key, &settings.region, &batch, &to)?,
            Service::Libre => libre(&agent, key, &settings.url, &batch, &to)?,
            Service::Off => return Err(i18n("Choose a translation service in Settings → Translation first.")),
        };
        if texts.len() != batch.len() {
            return Err(i18n("The service's answer could not be read."));
        }
        if from.is_none() {
            from = detected.map(|d| from_service_code(&d));
        }
        out.extend(texts);
    }
    Ok((out, from))
}

/// Translate what is being written (blocking): pieces of the composer's
/// own HTML, each kept whole so it goes back where it came from, into
/// `target`. Nothing is cached; the text is the user's, and changes.
pub fn translate_parts(settings: &Settings, key: &str, parts: &[String], target: &str) -> Result<Vec<String>, String> {
    if !settings.enabled() {
        return Err(i18n("Choose a translation service in Settings → Translation first."));
    }
    if key.is_empty() && settings.service != Service::Libre {
        return Err(i18n_f(
            "No key for {service}: add one in Settings → Translation.",
            &[("service", &settings.service.name())],
        ));
    }
    if parts.iter().map(String::len).sum::<usize>() > LIMIT {
        return Err(i18n("This is too long to send for translation."));
    }
    let (texts, _) = run(settings, key, parts, target)?;
    remember_compose_target(target);
    Ok(texts)
}

/// The language the composer last translated into this session, offered
/// first the next time.
static COMPOSE_TARGET: Mutex<String> = Mutex::new(String::new());

pub fn last_compose_target() -> Option<String> {
    COMPOSE_TARGET.lock().ok().map(|g| g.clone()).filter(|t| !t.is_empty())
}

fn remember_compose_target(target: &str) {
    if let Ok(mut g) = COMPOSE_TARGET.lock() {
        *g = target.to_string();
    }
}

/// Plain text as paragraphs of HTML, for a service asked to keep markup:
/// a blank line starts a paragraph, a line break stays a line break.
pub fn text_to_parts(text: &str) -> Vec<String> {
    text.split("\n\n")
        .map(|p| p.trim_matches('\n'))
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let esc = p.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            format!("<p>{}</p>", esc.replace('\n', "<br>"))
        })
        .collect()
}

/// [`text_to_parts`] the other way: the paragraphs that came back, as text.
pub fn parts_to_text(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            let p = p.replace("<br>", "\n").replace("<br/>", "\n").replace("<br />", "\n");
            let mut out = String::with_capacity(p.len());
            let mut in_tag = false;
            for c in p.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' if in_tag => in_tag = false,
                    _ if !in_tag => out.push(c),
                    _ => {}
                }
            }
            out.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&#39;", "'")
                .replace("&nbsp;", " ")
                .replace("&amp;", "&")
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Elements that sit inside a line of text. A run of these and text is
/// what goes to the service as one unit, so a sentence with a link or a
/// bold word in it is translated whole.
const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "br", "cite", "code", "data", "dfn", "em", "font", "i", "kbd", "label",
    "mark", "q", "s", "samp", "small", "span", "strike", "strong", "sub", "sup", "time", "tt", "u", "var",
    "wbr",
];
/// Elements whose content is never sent.
const UNSENT: &[&str] = &[
    "script", "style", "head", "title", "noscript", "template", "svg", "math", "textarea", "select",
];
/// The attribute that numbers each run in a piece, and the one that stands
/// in for an inline element's own attributes while it is away: an `href`
/// full of tracking, a `style`, cost the user's allowance and say nothing.
const RUN_ATTR: &str = "data-hylki-run";
const ATTRS_ATTR: &str = "data-hylki-a";

/// A message being translated where it stands (#327): its parsed document,
/// the runs of text in it, and the pieces of HTML they are sent as.
struct InPlace {
    dom: markup5ever_rcdom::RcDom,
    /// Each run: its parent and the range of the parent's children.
    runs: Vec<(markup5ever_rcdom::Handle, std::ops::Range<usize>)>,
    /// The inline elements' attributes, by the number they were sent with.
    attrs: Vec<Vec<html5ever::Attribute>>,
    pieces: Vec<String>,
}

impl InPlace {
    fn parse(body: &str) -> InPlace {
        use html5ever::tendril::TendrilSink;
        let dom = html5ever::parse_document(markup5ever_rcdom::RcDom::default(), Default::default()).one(body);
        let mut runs = Vec::new();
        collect_runs(&dom.document, &mut runs);
        let mut attrs = Vec::new();
        let mut pieces: Vec<String> = Vec::new();
        for (i, (parent, range)) in runs.iter().enumerate() {
            let mut html = format!("<div {RUN_ATTR}=\"{i}\">");
            for node in &parent.children.borrow()[range.clone()] {
                stand_in_attrs(node, &mut attrs);
                html.push_str(&serialize(node));
            }
            html.push_str("</div>");
            match pieces.last_mut() {
                Some(last) if last.len() + html.len() <= CHUNK => last.push_str(&html),
                _ => pieces.push(html),
            }
        }
        InPlace { dom, runs, attrs, pieces }
    }

    /// The document with each run replaced by its translation. A run the
    /// service did not send back keeps its own text.
    fn fill(&mut self, translated: &[String]) -> String {
        use html5ever::tendril::TendrilSink;
        let mut by_run: HashMap<usize, Vec<markup5ever_rcdom::Handle>> = HashMap::new();
        // Kept until the end: dropping an RcDom empties every node that was
        // in it, the ones moved into the message included.
        let mut answers = Vec::new();
        for piece in translated {
            let dom = html5ever::parse_document(markup5ever_rcdom::RcDom::default(), Default::default())
                .one(format!("<body>{piece}</body>"));
            find_runs(&dom.document, &mut by_run);
            answers.push(dom);
        }
        for (i, (parent, range)) in self.runs.iter().enumerate().rev() {
            let mut kids = parent.children.borrow_mut();
            match by_run.remove(&i) {
                Some(new) => {
                    for n in &new {
                        n.parent.set(Some(std::rc::Rc::downgrade(parent)));
                        restore_attrs(n, &self.attrs);
                    }
                    kids.splice(range.clone(), new);
                }
                None => {
                    for n in &kids[range.clone()] {
                        restore_attrs(n, &self.attrs);
                    }
                }
            }
        }
        let mut out = Vec::new();
        let doc: markup5ever_rcdom::SerializableHandle = self.dom.document.clone().into();
        let _ = html5ever::serialize(&mut out, &doc, Default::default());
        drop(answers);
        String::from_utf8_lossy(&out).into_owned()
    }
}

fn element_name(node: &markup5ever_rcdom::Handle) -> Option<String> {
    match &node.data {
        markup5ever_rcdom::NodeData::Element { name, .. } => Some(name.local.to_string().to_ascii_lowercase()),
        _ => None,
    }
}

fn is_hidden_node(node: &markup5ever_rcdom::Handle) -> bool {
    match &node.data {
        markup5ever_rcdom::NodeData::Element { attrs, .. } => crate::reader::is_hidden(&attrs.borrow()),
        _ => false,
    }
}

/// Text, or an inline element holding only text and inline elements.
fn is_inline(node: &markup5ever_rcdom::Handle) -> bool {
    use markup5ever_rcdom::NodeData;
    match &node.data {
        NodeData::Text { .. } | NodeData::Comment { .. } => true,
        NodeData::Element { .. } => {
            element_name(node).is_some_and(|n| INLINE.contains(&n.as_str()))
                && !is_hidden_node(node)
                && node.children.borrow().iter().all(is_inline)
        }
        _ => false,
    }
}

fn has_text(node: &markup5ever_rcdom::Handle) -> bool {
    match &node.data {
        markup5ever_rcdom::NodeData::Text { contents } => contents.borrow().chars().any(char::is_alphanumeric),
        _ => node.children.borrow().iter().any(has_text),
    }
}

/// Every run of inline content with words in it, in document order.
fn collect_runs(node: &markup5ever_rcdom::Handle, runs: &mut Vec<(markup5ever_rcdom::Handle, std::ops::Range<usize>)>) {
    let kids = node.children.borrow();
    let mut i = 0;
    while i < kids.len() {
        if is_inline(&kids[i]) {
            let start = i;
            while i < kids.len() && is_inline(&kids[i]) {
                i += 1;
            }
            if kids[start..i].iter().any(has_text) {
                runs.push((node.clone(), start..i));
            }
            continue;
        }
        let child = &kids[i];
        let sent = match element_name(child) {
            Some(name) => !UNSENT.contains(&name.as_str()) && !is_hidden_node(child),
            None => true,
        };
        if sent {
            collect_runs(child, runs);
        }
        i += 1;
    }
}

/// Swap an inline element's attributes, and its descendants', for a number.
fn stand_in_attrs(node: &markup5ever_rcdom::Handle, saved: &mut Vec<Vec<html5ever::Attribute>>) {
    if let markup5ever_rcdom::NodeData::Element { attrs, .. } = &node.data {
        let mut attrs = attrs.borrow_mut();
        if !attrs.is_empty() {
            let n = saved.len();
            saved.push(std::mem::take(&mut *attrs));
            attrs.push(html5ever::Attribute {
                name: html5ever::QualName::new(None, html5ever::ns!(), html5ever::LocalName::from(ATTRS_ATTR)),
                value: n.to_string().into(),
            });
        }
    }
    for c in node.children.borrow().iter() {
        stand_in_attrs(c, saved);
    }
}

/// Put back the attributes [`stand_in_attrs`] took.
fn restore_attrs(node: &markup5ever_rcdom::Handle, saved: &[Vec<html5ever::Attribute>]) {
    if let markup5ever_rcdom::NodeData::Element { attrs, .. } = &node.data {
        let mut attrs = attrs.borrow_mut();
        let n = attrs
            .iter()
            .find(|a| &*a.name.local == ATTRS_ATTR)
            .and_then(|a| a.value.parse::<usize>().ok());
        attrs.retain(|a| &*a.name.local != ATTRS_ATTR);
        if let Some(original) = n.and_then(|n| saved.get(n)) {
            *attrs = original.clone();
        }
    }
    for c in node.children.borrow().iter() {
        restore_attrs(c, saved);
    }
}

/// The numbered runs in a piece that came back: each one's children.
fn find_runs(node: &markup5ever_rcdom::Handle, out: &mut HashMap<usize, Vec<markup5ever_rcdom::Handle>>) {
    if let markup5ever_rcdom::NodeData::Element { attrs, .. } = &node.data {
        let n = attrs
            .borrow()
            .iter()
            .find(|a| &*a.name.local == RUN_ATTR)
            .and_then(|a| a.value.trim().parse::<usize>().ok());
        if let Some(n) = n {
            out.insert(n, node.children.borrow().clone());
            return;
        }
    }
    for c in node.children.borrow().iter() {
        find_runs(c, out);
    }
}

fn serialize(node: &markup5ever_rcdom::Handle) -> String {
    let mut out = Vec::new();
    let handle: markup5ever_rcdom::SerializableHandle = node.clone().into();
    let opts = html5ever::serialize::SerializeOpts {
        traversal_scope: html5ever::serialize::TraversalScope::IncludeNode,
        ..Default::default()
    };
    let _ = html5ever::serialize(&mut out, &handle, opts);
    String::from_utf8_lossy(&out).into_owned()
}

/// The Reader View of a body as top-level blocks. Plain text, which the
/// reader shows line for line, is turned into paragraphs and line breaks
/// first: sent as markup, its newlines would otherwise be read as spaces.
fn prepare_blocks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for block in crate::reader::extract_blocks(body) {
        match block
            .strip_prefix("<div class=\"hylki-plain\">")
            .and_then(|b| b.strip_suffix("</div>"))
        {
            Some(text) => {
                for para in text.split("\n\n").map(|p| p.trim_matches('\n')).filter(|p| !p.trim().is_empty()) {
                    out.push(format!("<p>{}</p>", para.replace('\n', "<br>")));
                }
            }
            None if !block.trim().is_empty() => out.push(block),
            None => {}
        }
    }
    out
}

/// Blocks joined into pieces of at most [`CHUNK`] characters; a block
/// bigger than that on its own goes as one piece.
fn chunk(blocks: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in blocks {
        match out.last_mut() {
            Some(last) if last.len() + b.len() <= CHUNK => last.push_str(b),
            _ => out.push(b.clone()),
        }
    }
    out
}

/// Pieces grouped into requests. LibreTranslate servers differ in whether
/// they take several texts at once, so it gets one at a time.
fn batches(chunks: &[String], service: Service) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut size = 0;
    for c in chunks {
        let full = match out.last() {
            None => true,
            Some(last) => service == Service::Libre || size + c.len() > BATCH || last.len() >= 50,
        };
        if full {
            out.push(Vec::new());
            size = 0;
        }
        size += c.len();
        out.last_mut().unwrap().push(c.clone());
    }
    out
}

type Answer = (Vec<String>, Option<String>);

/// A request's result as JSON, with the service's own explanation when it
/// refused.
fn send(req: ureq::Request, body: Value) -> Result<Value, String> {
    match req.send_json(body) {
        Ok(resp) => resp.into_json::<Value>().map_err(|_| i18n("The service's answer could not be read.")),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Err(refusal(code, &text))
        }
        Err(e) => Err(i18n_f("The service could not be reached: {e}", &[("e", &e.to_string())])),
    }
}

/// What a refusal means, in words.
fn refusal(code: u16, body: &str) -> String {
    let said = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            [
                v["message"].as_str(),
                v["error"]["message"].as_str(),
                v["error"].as_str(),
                v["detail"].as_str(),
            ]
            .into_iter()
            .flatten()
            .next()
            .map(str::to_string)
        })
        .unwrap_or_default();
    let what = match code {
        401 | 403 => i18n("The service did not accept the key."),
        429 => i18n("Too many requests: try again in a moment."),
        456 => i18n("This month's allowance is used up."),
        _ => i18n_f("The service refused the request (error {code}).", &[("code", &code.to_string())]),
    };
    if said.is_empty() {
        what
    } else {
        format!("{what} ({said})")
    }
}

fn deepl(agent: &ureq::Agent, key: &str, texts: &[String], to: &str) -> Result<Answer, String> {
    let host = if key.trim().ends_with(":fx") { "api-free.deepl.com" } else { "api.deepl.com" };
    let req = agent
        .post(&format!("https://{host}/v2/translate"))
        .set("Authorization", &format!("DeepL-Auth-Key {}", key.trim()));
    let v = send(req, json!({ "text": texts, "target_lang": to, "tag_handling": "html" }))?;
    let list = v["translations"].as_array().cloned().unwrap_or_default();
    let detected = list.first().and_then(|t| t["detected_source_language"].as_str()).map(str::to_string);
    Ok((list.iter().map(|t| t["text"].as_str().unwrap_or_default().to_string()).collect(), detected))
}

fn google(agent: &ureq::Agent, key: &str, texts: &[String], to: &str) -> Result<Answer, String> {
    let req = agent
        .post("https://translation.googleapis.com/language/translate/v2")
        .query("key", key.trim());
    let v = send(req, json!({ "q": texts, "target": to, "format": "html" }))?;
    let list = v["data"]["translations"].as_array().cloned().unwrap_or_default();
    let detected = list.first().and_then(|t| t["detectedSourceLanguage"].as_str()).map(str::to_string);
    Ok((list.iter().map(|t| t["translatedText"].as_str().unwrap_or_default().to_string()).collect(), detected))
}

fn microsoft(agent: &ureq::Agent, key: &str, region: &str, texts: &[String], to: &str) -> Result<Answer, String> {
    let mut req = agent
        .post("https://api.cognitive.microsofttranslator.com/translate")
        .query("api-version", "3.0")
        .query("to", to)
        .query("textType", "html")
        .set("Ocp-Apim-Subscription-Key", key.trim());
    if !region.trim().is_empty() {
        req = req.set("Ocp-Apim-Subscription-Region", region.trim());
    }
    let body: Vec<Value> = texts.iter().map(|t| json!({ "Text": t })).collect();
    let v = send(req, Value::Array(body))?;
    let list = v.as_array().cloned().unwrap_or_default();
    let detected = list.first().and_then(|t| t["detectedLanguage"]["language"].as_str()).map(str::to_string);
    Ok((list.iter().map(|t| t["translations"][0]["text"].as_str().unwrap_or_default().to_string()).collect(), detected))
}

fn libre(agent: &ureq::Agent, key: &str, url: &str, texts: &[String], to: &str) -> Result<Answer, String> {
    let base = url.trim().trim_end_matches('/');
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err(i18n("Enter the LibreTranslate server's address in Settings → Translation."));
    }
    let mut out = Vec::new();
    let mut detected = None;
    for text in texts {
        let mut body = json!({ "q": text, "source": "auto", "target": to, "format": "html" });
        if !key.trim().is_empty() {
            body["api_key"] = Value::String(key.trim().to_string());
        }
        let v = send(agent.post(&format!("{base}/translate")), body)?;
        if detected.is_none() {
            detected = v["detectedLanguage"]["language"].as_str().map(str::to_string);
        }
        out.push(v["translatedText"].as_str().unwrap_or_default().to_string());
    }
    Ok((out, detected))
}

/// Try the settings with a greeting, for the Settings page's check
/// (blocking): what was sent and what came back. The greeting is in a
/// language other than the target, or nothing would change.
pub fn check(settings: &Settings, key: &str) -> Result<(String, String), String> {
    let greeting = if base(&settings.target_language()) == "en" { "Guten Morgen" } else { "Good morning" };
    let probe = format!("{}\u{1f}check", generation());
    let t = translate(settings, key, &format!("<p>{greeting}</p>"), "", &probe)?;
    if let Ok(mut g) = DONE.lock() {
        if let Some(map) = g.as_mut() {
            map.remove(&probe);
        }
    }
    Ok((greeting.to_string(), visible_text(&t.html).trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in service: upper-cases the text, keeps every tag as sent.
    fn shout(piece: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for c in piece.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ => {}
            }
            out.extend(if in_tag || c == '>' { c.to_lowercase().collect::<Vec<_>>() } else { c.to_uppercase().collect() });
        }
        out
    }

    #[test]
    fn a_translation_keeps_the_messages_design() {
        // #327: what was sent was Reader View, so it came back unstyled.
        let body = r##"<!doctype html><html><head><style>.hero{color:#c00}</style></head><body>
            <div style="display:none">preheader text</div>
            <table class="hero" bgcolor="#eee"><tr><td style="padding:20px">Hello <a href="https://example.com/track?id=42" style="color:red">our offer</a>, friend.</td></tr></table>
            <p>Second <b>line</b></p><img src="cid:logo"></body></html>"##;
        let mut doc = InPlace::parse(body);
        assert_eq!(doc.runs.len(), 2, "{:?}", doc.pieces);
        let sent = doc.pieces.concat();
        assert!(!sent.contains("preheader") && !sent.contains("example.com") && !sent.contains(".hero"), "{sent}");
        let back: Vec<String> = doc.pieces.iter().map(|p| shout(p)).collect();
        let html = doc.fill(&back);
        assert!(html.contains("<style>.hero{color:#c00}</style>"), "{html}");
        assert!(html.contains(r#"<td style="padding:20px">HELLO <a href="https://example.com/track?id=42" style="color:red">OUR OFFER</a>, FRIEND.</td>"#), "{html}");
        assert!(html.contains("<p>SECOND <b>LINE</b></p>"), "{html}");
        assert!(html.contains("preheader text") && html.contains(r##"bgcolor="#eee""##), "{html}");
        assert!(!html.contains(RUN_ATTR) && !html.contains(ATTRS_ATTR), "{html}");
    }

    /// The same against a real LibreTranslate server (#327):
    /// `LIBRE_LIVE=http://localhost:5000 cargo test -- --ignored live_libre`.
    #[test]
    #[ignore]
    fn live_libre_keeps_the_design() {
        let Ok(url) = std::env::var("LIBRE_LIVE") else { return };
        let settings = Settings { service: Service::Libre, target: "de".into(), url, ..Default::default() };
        let body = r##"<html><head><style>.hero{color:#c00}</style></head><body><table class="hero"><tr><td style="padding:20px">Good morning, <a href="https://example.com/x">read our news</a> today.</td></tr></table><p>Thank you for your <b>order</b>.</p></body></html>"##;
        let t = translate(&settings, "", body, "Good morning", "live-libre-test").expect("translated");
        eprintln!("{}", t.html);
        assert!(!t.reader);
        assert!(t.html.contains("<style>.hero{color:#c00}</style>"));
        assert!(t.html.contains(r#"<td style="padding:20px">"#) && t.html.contains(r#"href="https://example.com/x""#));
        assert!(!t.html.contains("Good morning") && !t.html.contains(RUN_ATTR) && !t.html.contains(ATTRS_ATTR));
        let subject = t.subject.expect("the subject came back");
        eprintln!("subject: {subject}");
        assert!(!subject.is_empty() && subject != "Good morning");
    }

    #[test]
    fn a_run_that_does_not_come_back_keeps_its_text() {
        let mut doc = InPlace::parse("<p>One <a href=\"x\">two</a></p><p>Three</p>");
        let html = doc.fill(&[String::new()]);
        assert!(html.contains("<p>One <a href=\"x\">two</a></p><p>Three</p>"), "{html}");
    }

    #[test]
    fn locales_become_target_codes() {
        assert_eq!(normalize("de_AT.UTF-8"), "de");
        assert_eq!(normalize("pt_BR.UTF-8"), "pt-BR");
        assert_eq!(normalize("pt_PT"), "pt-PT");
        assert_eq!(normalize("zh_TW.UTF-8"), "zh-Hant");
        assert_eq!(normalize("zh_CN"), "zh-Hans");
        assert_eq!(normalize("nn_NO"), "nb");
        assert_eq!(normalize("en"), "en");
    }

    #[test]
    fn every_service_gets_its_own_spelling() {
        assert_eq!(service_code(Service::DeepL, "pt-BR"), "PT-BR");
        assert_eq!(service_code(Service::DeepL, "zh-Hans"), "ZH-HANS");
        assert_eq!(service_code(Service::Google, "zh-Hant"), "zh-TW");
        assert_eq!(service_code(Service::Google, "he"), "iw");
        assert_eq!(service_code(Service::Microsoft, "pt-PT"), "pt-pt");
        assert_eq!(service_code(Service::Libre, "pt-BR"), "pt");
        assert_eq!(service_code(Service::Microsoft, "de"), "de");
        for code in LANGUAGES {
            assert!(!language_name(code).is_empty());
        }
    }

    #[test]
    fn detected_languages_come_back_as_ours() {
        assert_eq!(from_service_code("DE"), "de");
        assert_eq!(from_service_code("EN-US"), "en");
        assert_eq!(from_service_code("zh-CN"), "zh-Hans");
        assert_eq!(from_service_code("zh-TW"), "zh-Hant");
        assert_eq!(from_service_code("iw"), "he");
    }

    #[test]
    fn plain_text_is_sent_as_paragraphs() {
        let blocks = prepare_blocks("Hallo Anna,\nwie geht es?\n\nViele Grüße\nBen");
        assert_eq!(blocks, ["<p>Hallo Anna,<br>wie geht es?</p>", "<p>Viele Grüße<br>Ben</p>"]);
    }

    #[test]
    fn long_messages_go_in_pieces() {
        let blocks: Vec<String> = (0..30).map(|i| format!("<p>{}</p>", "x".repeat(400 + i))).collect();
        let chunks = chunk(&blocks);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= CHUNK));
        assert_eq!(chunks.concat(), blocks.concat(), "nothing lost or reordered");
        assert_eq!(batches(&chunks, Service::DeepL).len(), 1);
        assert_eq!(batches(&chunks, Service::Libre).len(), chunks.len());
    }

    #[test]
    fn the_language_is_told_offline_and_quotes_do_not_count() {
        let de = "<p>Vielen Dank für Ihre Nachricht. Wir melden uns so bald wie möglich bei Ihnen, \
                  spätestens aber bis Ende der Woche.</p>";
        assert_eq!(detect(de).as_deref(), Some("de"));
        let reply = format!(
            "<p>Thanks, that works for me. I will bring the printed plans and the budget, and we can go through \
             both before lunch. See you on Friday at the office then.</p><blockquote>{de}{de}{de}</blockquote>"
        );
        assert_eq!(detect(&reply).as_deref(), Some("en"));
        assert_eq!(detect("<p>OK</p>"), None, "too little to go on");
        assert!(differs("de", "en"));
        assert!(!differs("pt", "pt-BR"));
    }

    #[test]
    fn written_text_goes_as_paragraphs_and_comes_back_as_text() {
        let parts = text_to_parts("Hi Anna,\nthanks <3\n\n**Ben**\n");
        assert_eq!(parts, ["<p>Hi Anna,<br>thanks &lt;3</p>", "<p>**Ben**</p>"]);
        let back = parts_to_text(&["<p>Hallo Anna,<br/>danke &lt;3</p>".to_string(), "<p>**Ben**</p>".to_string()]);
        assert_eq!(back, "Hallo Anna,\ndanke <3\n\n**Ben**");
    }

    #[test]
    fn refusals_say_what_went_wrong() {
        assert!(refusal(403, r#"{"message":"Wrong key"}"#).contains("Wrong key"));
        assert!(!refusal(456, "").is_empty());
        assert!(refusal(400, r#"{"error":{"message":"Bad language"}}"#).contains("Bad language"));
    }
}
