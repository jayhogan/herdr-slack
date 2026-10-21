//! Message text rendering (spec §8).

use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::model::Names;

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Text of a message: `text`, else its blocks, else its attachments (spec §8).
/// With none readable, a label so the row is never blank.
pub fn flatten(v: &Value) -> String {
    let text = s(v, "text");
    if !is_blank(text) {
        return text.to_owned();
    }
    let from_blocks = v
        .get("blocks")
        .and_then(Value::as_array)
        .map(|bs| join_nonempty(bs.iter().map(block_text)))
        .unwrap_or_default();
    if !is_blank(&from_blocks) {
        return from_blocks;
    }
    let from_attachments = v
        .get("attachments")
        .and_then(Value::as_array)
        .map(|az| join_nonempty(az.iter().map(attachment_text)))
        .unwrap_or_default();
    if !is_blank(&from_attachments) {
        return from_attachments;
    }
    let has_files = v
        .get("files")
        .and_then(Value::as_array)
        .is_some_and(|fs| !fs.is_empty());
    if has_files {
        "[Files were uploaded]"
    } else {
        "[Rich content]"
    }
    .to_owned()
}

/// Bots often send `text` as a space or zero-width character to carry a card.
fn is_blank(t: &str) -> bool {
    t.chars().all(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}'
            )
    })
}

fn join_nonempty(parts: impl Iterator<Item = String>) -> String {
    parts
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn nested_text(v: &Value) -> String {
    v.get("text")
        .and_then(|t| t.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn block_text(b: &Value) -> String {
    match s(b, "type") {
        "rich_text" => b
            .get("elements")
            .and_then(Value::as_array)
            .map(|es| join_nonempty(es.iter().map(rich_element)))
            .unwrap_or_default(),
        "section" => {
            let fields = b
                .get("fields")
                .and_then(Value::as_array)
                .map(|fs| fs.iter().map(|f| s(f, "text").to_owned()));
            join_nonempty(std::iter::once(nested_text(b)).chain(fields.into_iter().flatten()))
        }
        "header" => nested_text(b),
        "context" => b
            .get("elements")
            .and_then(Value::as_array)
            .map(|es| join_nonempty(es.iter().map(|e| s(e, "text").to_owned())))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn rich_element(e: &Value) -> String {
    let children = || {
        e.get("elements")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    };
    match s(e, "type") {
        "text" => s(e, "text").to_owned(),
        "user" => format!("<@{}>", s(e, "user_id")),
        "channel" => format!("<#{}>", s(e, "channel_id")),
        "usergroup" => format!("<!subteam^{}>", s(e, "usergroup_id")),
        "broadcast" => format!("<!{}>", s(e, "range")),
        "link" => match s(e, "text") {
            "" => s(e, "url").to_owned(),
            t => t.to_owned(),
        },
        "emoji" => format!(":{}:", s(e, "name")),
        "rich_text_list" => join_nonempty(children().iter().map(rich_element)),
        _ => children().iter().map(rich_element).collect(),
    }
}

fn attachment_text(a: &Value) -> String {
    let parts = join_nonempty(
        ["pretext", "title", "text"]
            .iter()
            .map(|k| s(a, k).to_owned()),
    );
    if parts.is_empty() {
        s(a, "fallback").to_owned()
    } else {
        parts
    }
}

/// Each `<...>` token in `body`: (byte range, inner text).
fn tokens(body: &str) -> impl Iterator<Item = &str> {
    let mut rest = body;
    std::iter::from_fn(move || {
        let start = rest.find('<')?;
        let end = start + rest[start..].find('>')?;
        let inner = &rest[start + 1..end];
        rest = &rest[end + 1..];
        Some(inner)
    })
}

/// User ids and channel ids referenced by `<@U…>` / `<#C…>` tokens, deduplicated, in order.
pub fn ids(body: &str) -> (Vec<String>, Vec<String>) {
    let (mut users, mut chans) = (Vec::new(), Vec::new());
    for t in tokens(body) {
        let (list, rest) = if let Some(r) = t.strip_prefix('@') {
            (&mut users, r)
        } else if let Some(r) = t.strip_prefix('#') {
            (&mut chans, r)
        } else {
            continue;
        };
        let id = rest.split('|').next().unwrap_or("");
        if !id.is_empty() && !list.iter().any(|x| x == id) {
            list.push(id.to_owned());
        }
    }
    (users, chans)
}

fn render_token(t: &str, names: &Names) -> Option<String> {
    let (head, label) = match t.split_once('|') {
        Some((h, l)) => (h, Some(l)),
        None => (t, None),
    };
    if let Some(id) = head.strip_prefix('@') {
        return Some(match names.users.get(id) {
            Some(n) => format!("@{n}"),
            None => format!("@{}", label.filter(|l| !l.is_empty()).unwrap_or(id)),
        });
    }
    if let Some(id) = head.strip_prefix('#') {
        if let Some(n) = names.channels.get(id) {
            return Some(n.clone());
        }
        return Some(format!(
            "#{}",
            label.filter(|l| !l.is_empty()).unwrap_or(id)
        ));
    }
    if let Some(cmd) = head.strip_prefix('!') {
        if let Some(l) = label.filter(|l| !l.is_empty()) {
            return Some(l.to_owned());
        }
        let word = cmd.split('^').next().unwrap_or(cmd);
        return Some(format!("@{word}"));
    }
    if head.contains(':') {
        return Some(label.filter(|l| !l.is_empty()).unwrap_or(head).to_owned());
    }
    None
}

/// Resolve Slack tokens and entities to readable text (spec §8).
pub fn display(body: &str, names: &Names) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        let end = start + len;
        out.push_str(&rest[..start]);
        match render_token(&rest[start + 1..end], names) {
            Some(r) => out.push_str(&r),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Collapse all whitespace runs to single spaces.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Width in terminal cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Byte length of the longest prefix of `s` that measures at most `w` cells (str-level width).
fn prefix_end(s: &str, w: usize) -> usize {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let next = i + c.len_utf8();
        if width(&s[..next]) > w {
            break;
        }
        end = next;
    }
    end
}

/// Fit `s` in `w` cells, ending with `…` when cut.
pub fn truncate(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_owned();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = s[..prefix_end(s, w - 1)].to_owned();
    out.push('…');
    out
}

/// Word-wrap to `w` cells, at most `max_lines` lines; the last line ends in `…` if text was cut.
/// Newlines in `s` start new lines.
pub fn wrap(s: &str, w: usize, max_lines: usize) -> Vec<String> {
    let w = w.max(1);
    let mut lines: Vec<String> = Vec::new();
    for para in s.split('\n') {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            let mut word = word;
            loop {
                let sep = usize::from(!cur.is_empty());
                if width(&cur) + sep + width(word) <= w {
                    if sep == 1 {
                        cur.push(' ');
                    }
                    cur.push_str(word);
                    break;
                }
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    continue;
                }
                // Word wider than a line: split it by cells.
                let mut cut = prefix_end(word, w);
                if cut == 0 {
                    cut = word.chars().next().map_or(word.len(), char::len_utf8);
                }
                lines.push(word[..cut].to_owned());
                word = &word[cut..];
                if word.is_empty() {
                    break;
                }
            }
        }
        lines.push(cur);
    }
    if lines.len() > max_lines {
        let tail = lines.split_off(max_lines.saturating_sub(1));
        if max_lines > 0 {
            let joined = tail
                .iter()
                .map(|l| l.as_str())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(truncate(&format!("{joined} "), w));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn flatten_prefers_text() {
        let v = serde_json::json!({"text": "plain", "blocks": [{"type":"header","text":{"text":"nope"}}]});
        assert_eq!(flatten(&v), "plain");
    }

    #[test]
    fn flatten_uses_blocks_when_text_empty() {
        let s = flatten(&fixture(include_str!("../tests/fixtures/msg-rich.json")));
        assert!(s.contains("Deploy blocked: core-api"), "{s}");
        assert!(s.contains("<@U0ME>"), "{s}");
        assert!(s.contains("Posted by Release Bot"), "{s}");
    }

    #[test]
    fn flatten_rich_text_elements() {
        let v = serde_json::json!({"text": "", "blocks": [{"type": "rich_text", "elements": [
        {"type": "rich_text_section", "elements": [
            {"type": "text", "text": "ping "}, {"type": "user", "user_id": "U0ME"},
            {"type": "text", "text": " in "}, {"type": "channel", "channel_id": "C0CORE"},
            {"type": "usergroup", "usergroup_id": "S1"}, {"type": "broadcast", "range": "here"},
            {"type": "link", "url": "https://x.io", "text": "doc"}, {"type": "emoji", "name": "eyes"}
        ]}]}]});
        assert_eq!(
            flatten(&v),
            "ping <@U0ME> in <#C0CORE><!subteam^S1><!here>doc:eyes:"
        );
    }

    #[test]
    fn flatten_uses_attachments_last() {
        let s = flatten(&fixture(include_str!("../tests/fixtures/msg-attach.json")));
        assert!(s.contains("Error rate high"), "{s}");
        assert!(s.contains("5xx rate is 4% on core-api"), "{s}");
    }

    #[test]
    fn flatten_skips_blank_text() {
        let v = serde_json::json!({"text": " \u{200b}\n", "blocks": [{"type":"header","text":{"text":"real"}}]});
        assert_eq!(flatten(&v), "real");
    }

    #[test]
    fn flatten_labels_unreadable_cards() {
        let s = flatten(&fixture(include_str!("../tests/fixtures/msg-card.json")));
        assert_eq!(s, "[Rich content]");
    }

    #[test]
    fn flatten_labels_file_uploads() {
        let s = flatten(&fixture(include_str!("../tests/fixtures/msg-files.json")));
        assert_eq!(s, "[Files were uploaded]");
    }

    #[test]
    fn display_resolves_tokens() {
        let mut names = Names::default();
        names.users.insert("U0MARCUS".into(), "Marcus Lee".into());
        names.channels.insert("C0CORE".into(), "#core-eng".into());
        assert_eq!(
            display(
                "hey <@U0MARCUS> see <#C0CORE> and <#C0X|ops> <!subteam^S1|@core> <!here> <https://x.io|doc> <https://y.io> a &amp; b &lt;c&gt;",
                &names
            ),
            "hey @Marcus Lee see #core-eng and #ops @core @here doc https://y.io a & b <c>"
        );
        assert_eq!(display("<@U0NOBODY>", &names), "@U0NOBODY");
    }

    #[test]
    fn ids_lists_users_and_channels() {
        let (u, c) = ids("<@U0A> <@U0B|bee> <@U0A> <#C0X|ops> <#C0Y>");
        assert_eq!(u, vec!["U0A", "U0B"]);
        assert_eq!(c, vec!["C0X", "C0Y"]);
    }

    #[test]
    fn one_line_collapses() {
        assert_eq!(one_line("a\n\n b\t c"), "a b c");
    }

    #[test]
    fn truncate_counts_cells() {
        assert_eq!(truncate("日本語テキスト", 7), "日本語…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
        assert_eq!(width("日本"), 4);
    }

    #[test]
    fn truncate_uses_str_width_for_emoji_sequences() {
        assert!(width(&truncate("❤️❤️❤️", 5)) <= 5);
        assert_eq!(truncate("👍🏽ab", 4), "👍🏽ab");
        assert!(width(&truncate("👨‍👩‍👧👨‍👩‍👧👨‍👩‍👧", 5)) <= 5);
    }

    #[test]
    fn wrap_ellipsis_on_last_line() {
        assert_eq!(wrap("aa bb cc dd ee", 5, 2), vec!["aa bb", "cc d…"]);
        assert_eq!(wrap("a\nb", 5, 5), vec!["a", "b"]);
    }
}
