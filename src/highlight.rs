//! Lazy, bounded syntax coloring. Parsing runs off the window thread; edits
//! restart at a saved line state, while painting consumes only color ranges.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};
use syntect::{
    highlighting::{
        Color, HighlightState, Highlighter, RangedHighlightIterator, StyleModifier, Theme,
        ThemeItem, ThemeSettings,
    },
    parsing::{
        ParseState, ScopeStack, SyntaxDefinition, SyntaxReference, SyntaxSet, SyntaxSetBuilder,
    },
};

use crate::{
    app::Event,
    renderer::ColorSpan,
    theme::{AMBER, FG, GREEN, MUTED, RED},
    windows::WindowProxy,
};

const MAX_BYTES: usize = 256 * 1024;
const MAX_LINE_BYTES: usize = 4096;
const MAX_SPANS: usize = 16 * 1024;
const CHECKPOINT_LINES: usize = 64;
const MAX_CHECKPOINTS: usize = 128;
const WORK_BUDGET: Duration = Duration::from_millis(150);
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default)]
pub struct Highlights {
    pub spans: Vec<ColorSpan>,
    pub language: Option<String>,
    pub limited: bool,
}

struct Task {
    path: PathBuf,
    text: String,
    token: u64,
    due: Instant,
}

#[derive(Default)]
struct Slot {
    task: Option<Task>,
    clear: bool,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    ready: Condvar,
    generation: AtomicU64,
}

pub struct HighlightWorker {
    shared: Arc<Shared>,
}

impl HighlightWorker {
    pub fn new(proxy: WindowProxy) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("leafterm-highlight".into())
            .stack_size(1024 * 1024)
            .spawn(move || run(worker, proxy))?;
        Ok(Self { shared })
    }

    pub fn submit(&self, path: PathBuf, text: String) -> u64 {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut slot = self.shared.slot.lock();
        self.shared.generation.store(token, Ordering::Release);
        slot.task = Some(Task {
            path,
            text,
            token,
            due: Instant::now() + Duration::from_millis(50),
        });
        self.shared.ready.notify_one();
        token
    }

    pub fn clear(&self) {
        let mut slot = self.shared.slot.lock();
        self.shared.generation.store(
            NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
            Ordering::Release,
        );
        slot.task = None;
        slot.clear = true;
        self.shared.ready.notify_one();
    }
}

impl Drop for HighlightWorker {
    fn drop(&mut self) {
        self.clear();
        let mut slot = self.shared.slot.lock();
        slot.stopped = true;
        self.shared.ready.notify_one();
    }
}

fn run(shared: Arc<Shared>, proxy: WindowProxy) {
    let mut engine = Engine::default();
    loop {
        let task = {
            let mut slot = shared.slot.lock();
            loop {
                if slot.stopped {
                    return;
                }
                if slot.clear {
                    engine.cached = None;
                    slot.clear = false;
                }
                match slot.task.as_ref().map(|task| task.due) {
                    None => shared.ready.wait(&mut slot),
                    Some(due) if due > Instant::now() => {
                        shared.ready.wait_until(&mut slot, due);
                    }
                    Some(_) => break slot.task.take().unwrap(),
                }
            }
        };
        let cancelled = || shared.generation.load(Ordering::Acquire) != task.token;
        if let Some(result) = engine.highlight(&task.path, task.text, WORK_BUDGET, &cancelled)
            && !cancelled()
            && proxy
                .send_event(Event::Highlighted {
                    token: task.token,
                    result,
                })
                .is_err()
        {
            return;
        }
    }
}

#[derive(Clone)]
struct Checkpoint {
    byte: usize,
    line: usize,
    spans: usize,
    parse: ParseState,
    highlight: HighlightState,
    conflict: Option<ConflictContext>,
}

#[derive(Clone)]
struct ConflictContext {
    marker_len: usize,
    parse: ParseState,
    highlight: HighlightState,
}

struct CachedFile {
    path: PathBuf,
    syntax: String,
    text: String,
    spans: Vec<ColorSpan>,
    checkpoints: Vec<Checkpoint>,
}

#[derive(Default)]
struct Engine {
    cached: Option<CachedFile>,
    #[cfg(test)]
    parsed_lines: usize,
}

impl Engine {
    fn highlight(
        &mut self,
        path: &Path,
        text: String,
        budget: Duration,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Highlights> {
        #[cfg(test)]
        {
            self.parsed_lines = 0;
        }
        if cancelled() {
            return None;
        }
        if text.len() > MAX_BYTES {
            self.cached = None;
            return Some(Highlights {
                limited: true,
                ..Highlights::default()
            });
        }
        let Some((set, syntax, language)) = syntax_for(path, &text) else {
            self.cached = None;
            return Some(Highlights::default());
        };
        if cancelled() {
            return None;
        }
        let started = Instant::now();
        let highlighter = Highlighter::new(theme());
        let mut checkpoint = Checkpoint {
            byte: 0,
            line: 0,
            spans: 0,
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&highlighter, ScopeStack::new()),
            conflict: None,
        };
        let mut spans = Vec::new();
        let mut checkpoints = Vec::new();
        if let Some(old) = self.cached.take()
            && old.path == path
            && old.syntax == syntax.name
        {
            let unchanged = old
                .text
                .bytes()
                .zip(text.bytes())
                .take_while(|(old, new)| old == new)
                .count();
            if let Some(saved) = old.checkpoints.iter().rev().find(|c| c.byte <= unchanged) {
                checkpoint = saved.clone();
                spans.extend_from_slice(&old.spans[..saved.spans]);
                checkpoints.extend(
                    old.checkpoints
                        .into_iter()
                        .take_while(|c| c.byte < checkpoint.byte),
                );
            }
        }
        let mut offset = checkpoint.byte;
        let mut parse = checkpoint.parse;
        let mut state = checkpoint.highlight;
        let mut conflict = checkpoint.conflict;
        let mut limited = false;
        for (index, line) in text[offset..].split_inclusive('\n').enumerate() {
            if cancelled() {
                return None;
            }
            if line.len() > MAX_LINE_BYTES || started.elapsed() >= budget {
                limited = true;
                break;
            }
            let row = checkpoint.line + index;
            if row.is_multiple_of(CHECKPOINT_LINES) && checkpoints.len() < MAX_CHECKPOINTS {
                checkpoints.push(Checkpoint {
                    byte: offset,
                    line: row,
                    spans: spans.len(),
                    parse: parse.clone(),
                    highlight: state.clone(),
                    conflict: conflict.clone(),
                });
            }
            if let Some((kind, marker_len)) = conflict_marker(line) {
                if kind == b'<' && conflict.is_none() {
                    conflict = Some(ConflictContext {
                        marker_len,
                        parse: parse.clone(),
                        highlight: state.clone(),
                    });
                } else if let Some(context) = &conflict
                    && context.marker_len == marker_len
                {
                    // Each side starts in the language state preceding the
                    // conflict, so a quote/comment on one side cannot leak.
                    parse = context.parse.clone();
                    state = context.highlight.clone();
                    if kind == b'>' {
                        conflict = None;
                    }
                }
                offset += line.len();
                continue;
            }
            #[cfg(test)]
            {
                self.parsed_lines += 1;
            }
            let Ok(operations) = parse.parse_line(line, set) else {
                limited = true;
                break;
            };
            for (style, _, range) in
                RangedHighlightIterator::new(&mut state, &operations, line, &highlighter)
            {
                let color = rgba(style.foreground);
                if color == FG || range.is_empty() {
                    continue;
                }
                let range = offset + range.start..offset + range.end;
                // Do not merge across lines: checkpoint span counts must end at
                // their exact byte boundary, even when a comment spans lines.
                if let Some(previous) = spans.last_mut()
                    && previous.range.start >= offset
                    && previous.range.end == range.start
                    && previous.color == color
                {
                    previous.range.end = range.end;
                } else if spans.len() < MAX_SPANS {
                    spans.push(ColorSpan { range, color });
                } else {
                    limited = true;
                    break;
                }
            }
            if limited {
                break;
            }
            offset += line.len();
        }
        if cancelled() {
            return None;
        }
        self.cached = Some(CachedFile {
            path: path.to_owned(),
            syntax: syntax.name.clone(),
            text,
            spans: spans.clone(),
            checkpoints,
        });
        Some(Highlights {
            spans,
            language: Some(language),
            limited,
        })
    }
}

fn conflict_marker(line: &str) -> Option<(u8, usize)> {
    let body = line.trim_end_matches(['\r', '\n']);
    let kind = *body.as_bytes().first()?;
    if !matches!(kind, b'<' | b'|' | b'=' | b'>') {
        return None;
    }
    let count = body.bytes().take_while(|&byte| byte == kind).count();
    let rest = &body[count..];
    (count >= 7
        && (rest.is_empty() || rest.starts_with([' ', '\t']))
        && (kind != b'=' || rest.trim().is_empty()))
    .then_some((kind, count))
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syntax_for(
    path: &Path,
    text: &str,
) -> Option<(&'static SyntaxSet, &'static SyntaxReference, String)> {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "txt" | "log") {
        return None;
    }
    if extension == "toml" {
        let set = toml_syntaxes();
        return set
            .find_syntax_by_extension("toml")
            .map(|syntax| (set, syntax, "TOML".into()));
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let set = syntaxes();
    let syntax = set
        .find_syntax_by_extension(name)
        .or_else(|| set.find_syntax_by_extension(&extension));
    if let Some(syntax) = syntax.filter(|syntax| syntax.name != "Plain Text") {
        return Some((set, syntax, syntax.name.clone()));
    }
    if matches!(extension.as_str(), "ts" | "tsx" | "mts" | "cts") {
        return set
            .find_syntax_by_extension("js")
            .map(|syntax| (set, syntax, "TypeScript · JS grammar".into()));
    }
    if name == ".env" || name.starts_with(".env.") {
        return set
            .find_syntax_by_extension("sh")
            .map(|syntax| (set, syntax, "Shell".into()));
    }
    let first = text.lines().next().unwrap_or("");
    if first.len() <= MAX_LINE_BYTES && first.starts_with("#!") {
        return set
            .find_syntax_by_first_line(first)
            .map(|syntax| (set, syntax, syntax.name.clone()));
    }
    None
}

// The bundled grammars omit TOML. Keep its small grammar separate so opening
// common configuration files does not load the other language definitions.
fn toml_syntaxes() -> &'static SyntaxSet {
    static TOML: OnceLock<SyntaxSet> = OnceLock::new();
    TOML.get_or_init(|| {
        let mut builder = SyntaxSetBuilder::new();
        builder.add(
            SyntaxDefinition::load_from_str(TOML_GRAMMAR, true, None)
                .expect("embedded TOML grammar"),
        );
        builder.build()
    })
}

const TOML_GRAMMAR: &str = r####"%YAML 1.2
---
name: TOML
file_extensions: [toml]
scope: source.toml
contexts:
  main:
    - match: '#.*$'
      scope: comment.line.number-sign.toml
    - match: '"""'
      push: multiline-basic
    - match: "'''"
      push: multiline-literal
    - match: '"'
      push: basic
    - match: "'"
      push: literal
    - match: '^\s*\[\[?[^\]\r\n]+\]\]?'
      scope: entity.name.section.toml
    - match: '[A-Za-z0-9_-]+(?=\s*[.=])'
      scope: entity.other.attribute-name.toml
    - match: '\b(true|false|inf|nan)\b'
      scope: constant.language.toml
    - match: '(?<![\w])[-+]?(?:0[xob][0-9A-Fa-f_]+|[0-9][0-9_]*(?:\.[0-9_]+)?(?:[eE][-+]?[0-9_]+)?)'
      scope: constant.numeric.toml
  basic:
    - meta_scope: string.quoted.double.toml
    - match: '\\.'
      scope: constant.character.escape.toml
    - match: '"|$'
      pop: true
  literal:
    - meta_scope: string.quoted.single.toml
    - match: "'|$"
      pop: true
  multiline-basic:
    - meta_scope: string.quoted.triple.double.toml
    - match: '\\.'
      scope: constant.character.escape.toml
    - match: '"""'
      pop: true
  multiline-literal:
    - meta_scope: string.quoted.triple.single.toml
    - match: "'''"
      pop: true
"####;

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| Theme {
        name: Some("LeafTerm".into()),
        settings: ThemeSettings {
            foreground: Some(color(FG)),
            ..ThemeSettings::default()
        },
        scopes: [
            ("comment", MUTED),
            ("string", GREEN),
            ("constant, constant.numeric", AMBER),
            ("keyword, storage", [183, 163, 228, 255]),
            ("entity.name, support.function", [128, 199, 219, 255]),
            ("entity.name.tag, entity.other.attribute-name", GREEN),
            ("markup.heading, markup.bold", AMBER),
            ("invalid", RED),
        ]
        .into_iter()
        .map(|(scope, foreground)| ThemeItem {
            scope: scope.parse().expect("static syntax selector"),
            style: StyleModifier {
                foreground: Some(color(foreground)),
                ..StyleModifier::default()
            },
        })
        .collect(),
        ..Theme::default()
    })
}

fn color(value: [u8; 4]) -> Color {
    Color {
        r: value[0],
        g: value[1],
        b: value[2],
        a: value[3],
    }
}

fn rgba(value: Color) -> [u8; 4] {
    [value.r, value.g, value.b, value.a]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlight(engine: &mut Engine, name: &str, text: &str) -> Highlights {
        engine
            .highlight(
                Path::new(name),
                text.into(),
                Duration::from_secs(10),
                &|| false,
            )
            .unwrap()
    }

    #[test]
    fn multiline_comments_and_unicode_have_valid_colored_ranges() {
        let text = "/* first\n中文 e\u{301}\n*/\nfn main() { let name = \"叶子\"; }\n";
        let result = highlight(&mut Engine::default(), "sample.rs", text);
        assert!(!result.limited);
        assert!(!result.spans.is_empty());
        let middle = text.find("中文").unwrap();
        assert!(
            result
                .spans
                .iter()
                .any(|s| s.range.contains(&middle) && s.color == MUTED)
        );
        let string = text.find("叶子").unwrap();
        assert!(
            result
                .spans
                .iter()
                .any(|s| s.range.contains(&string) && s.color == GREEN)
        );
        let mut end = 0;
        for span in result.spans {
            assert!(span.range.start >= end);
            assert!(text.get(span.range.clone()).is_some());
            end = span.range.end;
        }
    }

    #[test]
    fn incremental_edit_and_undo_match_fresh_parsing_and_reuse_prefix() {
        let original = "let value = 1;\n".repeat(200) + "/* comment */\nlet end = 2;\n";
        let changed = original.replace("/* comment */", "/* comment");
        let mut engine = Engine::default();
        highlight(&mut engine, "sample.rs", &original);
        let incremental = highlight(&mut engine, "sample.rs", &changed);
        assert!(engine.parsed_lines <= CHECKPOINT_LINES);
        let fresh = highlight(&mut Engine::default(), "sample.rs", &changed);
        assert_eq!(incremental.spans, fresh.spans);
        let undone = highlight(&mut engine, "sample.rs", &original);
        let fresh = highlight(&mut Engine::default(), "sample.rs", &original);
        assert_eq!(undone.spans, fresh.spans);
    }

    #[test]
    fn limits_cancellation_and_unknown_files_fall_back_without_stale_colors() {
        let mut engine = Engine::default();
        assert!(
            highlight(&mut engine, "sample.rs", "fn main() {}\n")
                .language
                .is_some()
        );
        let plain = highlight(&mut engine, "notes.txt", "fn main() {}\n");
        assert!(plain.spans.is_empty() && plain.language.is_none());
        assert!(engine.cached.is_none());
        assert!(highlight(&mut engine, "data.json", &" ".repeat(MAX_BYTES + 1)).limited);
        assert!(highlight(&mut engine, "data.json", &" ".repeat(MAX_LINE_BYTES + 1)).limited);
        assert!(
            engine
                .highlight(
                    Path::new("sample.rs"),
                    "fn main() {}".into(),
                    Duration::ZERO,
                    &|| false
                )
                .unwrap()
                .limited
        );
        assert!(
            engine
                .highlight(
                    Path::new("sample.rs"),
                    "fn main() {}".into(),
                    WORK_BUDGET,
                    &|| true
                )
                .is_none()
        );
    }

    #[test]
    fn common_languages_and_shebang_are_detected() {
        for name in [
            "sample.rs",
            "sample.js",
            "sample.ts",
            "config.json",
            "config.toml",
            "config.yaml",
            "sample.py",
            "sample.sh",
            "README.md",
            ".env.local",
        ] {
            assert!(syntax_for(Path::new(name), "").is_some(), "missing {name}");
        }
        assert!(syntax_for(Path::new("script"), "#!/usr/bin/env python3\n").is_some());
        assert!(syntax_for(Path::new("sample.unknown"), "arbitrary text").is_none());
    }

    #[test]
    fn toml_handles_boolean_strings_comments_and_multiline_literals() {
        let text = "[feature]\nenabled = true # toggle\nname = \"叶子 # still string\"\ntext = '''first\n# inside literal\nlast'''\ncount = 12\n";
        let result = highlight(&mut Engine::default(), "config.toml", text);
        assert_eq!(result.language.as_deref(), Some("TOML"));
        assert!(!result.limited);
        for (needle, expected) in [
            ("true", AMBER),
            ("# toggle", MUTED),
            ("叶子", GREEN),
            ("# inside literal", GREEN),
            ("12", AMBER),
        ] {
            let position = text.find(needle).unwrap();
            assert!(
                result
                    .spans
                    .iter()
                    .any(|span| span.range.contains(&position) && span.color == expected),
                "missing color for {needle}"
            );
        }
    }

    #[test]
    fn conflict_sides_do_not_share_unclosed_comment_state_including_incremental_edits() {
        let text = "fn main() {}\n<<<<<<< HEAD\n/* unfinished\n".to_owned()
            + &"current comment\n".repeat(80)
            + "=======\nlet incoming = \"leaf\";\n>>>>>>> topic\nlet after = 1;\n";
        let mut engine = Engine::default();
        let result = highlight(&mut engine, "sample.rs", &text);
        let string = text.find("leaf").unwrap();
        assert!(
            result
                .spans
                .iter()
                .any(|span| span.range.contains(&string) && span.color == GREEN)
        );
        let marker = text.find("=======").unwrap();
        assert!(!result.spans.iter().any(|span| span.range.contains(&marker)));
        let changed = text.replace("leaf", "叶子");
        let updated = highlight(&mut engine, "sample.rs", &changed);
        assert_eq!(
            updated.spans,
            highlight(&mut Engine::default(), "sample.rs", &changed).spans
        );
    }

    #[test]
    fn span_budget_and_cancellation_bound_large_colored_documents() {
        let text = "[1, 2, 3, 4]\n".repeat(4300);
        let result = highlight(&mut Engine::default(), "data.json", &text);
        assert!(result.limited);
        assert!(result.spans.len() <= MAX_SPANS);
        let calls = std::cell::Cell::new(0);
        let cancelled = || {
            calls.set(calls.get() + 1);
            calls.get() > 6
        };
        assert!(
            Engine::default()
                .highlight(
                    Path::new("data.json"),
                    text,
                    Duration::from_secs(10),
                    &cancelled
                )
                .is_none()
        );
    }

    #[test]
    #[ignore = "manual release-mode timing, not a performance guarantee"]
    fn benchmark_typical_file_and_incremental_edits() {
        let text = "fn example() { let name = \"LeafTerm\"; let enabled = true; }\n".repeat(560);
        let mut engine = Engine::default();
        let start = Instant::now();
        let result = highlight(&mut engine, "sample.rs", &text);
        let initial = start.elapsed();
        let changed = text.replacen("true; }\nfn example()", "false; }\nfn example()", 1);
        let mut near_end = text.clone();
        near_end.insert(text.rfind("true").unwrap(), '!');
        let start = Instant::now();
        for revision in [&changed, &text, &near_end, &text] {
            highlight(&mut engine, "sample.rs", revision);
        }
        println!(
            "{} bytes; initial {:?}; four edits {:?}; {} spans (~{} bytes per copy), {} checkpoints",
            text.len(),
            initial,
            start.elapsed(),
            result.spans.len(),
            result.spans.capacity() * std::mem::size_of::<ColorSpan>(),
            engine.cached.as_ref().unwrap().checkpoints.len()
        );
    }
}
