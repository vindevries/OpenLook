//! Small helpers: time formatting and HTML handling.

use chrono::{DateTime, Local, Utc};

pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}

fn parse(iso: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(iso).ok().map(|dt| dt.with_timezone(&Local))
}

/// Message-list timestamp, in the style Outlook uses: time today, weekday
/// this week, then dates.
pub fn fmt_time(iso: &str) -> String {
    let Some(dt) = parse(iso) else { return String::new() };
    let now = Local::now();
    let age = now.signed_duration_since(dt);
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else if age.num_days() < 7 && age.num_seconds() >= 0 {
        dt.format("%a %H:%M").to_string()
    } else if dt.format("%Y").to_string() == now.format("%Y").to_string() {
        dt.format("%-d %b").to_string()
    } else {
        dt.format("%d-%m-%Y").to_string()
    }
}

/// Full timestamp for the reading pane header.
pub fn fmt_full_time(iso: &str) -> String {
    parse(iso).map(|dt| dt.format("%A %-d %B %Y, %H:%M").to_string()).unwrap_or_default()
}

/// "just now" / "5 min ago" / "14:03", for the sync status line.
pub fn fmt_since(unix: i64) -> String {
    let secs = now_unix() - unix;
    if secs < 90 {
        "just now".into()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 6 * 3600 {
        format!("{}h ago", secs / 3600)
    } else {
        DateTime::from_timestamp(unix, 0)
            .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
            .unwrap_or_default()
    }
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Crude tag strip, used to build previews from HTML bodies and to show
/// mail when the WebKit view is unavailable.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skip_until: Option<&str> = None;
    let lower = html.to_lowercase();
    let bytes: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(tag) = skip_until {
            // Inside <style>/<script>: skip to the closing tag.
            let rest: String = lower_chars[i..].iter().take(tag.len()).collect();
            if rest == tag {
                skip_until = None;
                i += tag.len();
                in_tag = false;
                continue;
            }
            i += 1;
            continue;
        }
        let c = bytes[i];
        if c == '<' {
            let ahead: String = lower_chars[i..].iter().take(7).collect();
            if ahead.starts_with("<style") {
                skip_until = Some("</style>");
                i += 6;
                continue;
            }
            if ahead.starts_with("<script") {
                skip_until = Some("</script>");
                i += 7;
                continue;
            }
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
            out.push(' ');
        } else if !in_tag {
            out.push(c);
        }
        i += 1;
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Wrap a mail body in a document with readable typography that follows the
/// desktop light/dark theme.
pub fn wrap_body(is_html: bool, content: &str, dark: bool) -> String {
    let content = if is_html {
        content.to_string()
    } else {
        format!("<pre>{}</pre>", escape_html(content))
    };
    let (fg, bg, quote, link) = if dark {
        ("#e3e3e3", "#1e1e1e", "#9a9a9a", "#7cb7f2")
    } else {
        ("#1a1a1a", "#ffffff", "#555555", "#0f6cbd")
    };
    format!(
        "<!doctype html><html><head><meta charset='utf-8'>\
         <meta name='viewport' content='width=device-width, initial-scale=1'><style>\
         body{{font-family:'Segoe UI',Ubuntu,Cantarell,sans-serif;font-size:14px;\
         line-height:1.5;color:{fg};background:{bg};margin:16px;\
         overflow-wrap:break-word;}}\
         pre{{white-space:pre-wrap;font:inherit;}}\
         img{{max-width:100%;height:auto;}}\
         table{{max-width:100%;}}\
         blockquote{{border-left:3px solid {quote};margin-left:0;padding-left:12px;color:{quote};}}\
         a{{color:{link};}}\
         </style></head><body>{content}</body></html>"
    )
}

/// Render a mail thread as one document: every message in order, each under
/// a small header. A thread of one renders as just its body, so ordinary
/// mail looks exactly as it did.
pub fn wrap_thread(
    messages: &[(String, String, bool, String)],
    dark: bool,
) -> String {
    if let [(_, _, is_html, body)] = messages {
        return wrap_body(*is_html, body, dark);
    }
    let divider = if dark { "#3a3a3a" } else { "#e0e0e0" };
    let muted = if dark { "#9a9a9a" } else { "#5f5f5f" };
    let mut content = String::new();
    for (who, when, is_html, body) in messages {
        content.push_str(&format!(
            "<div style='border-top:1px solid {divider};margin-top:14px;padding-top:10px'>\
             <div style='color:{muted};font-size:0.9em;margin-bottom:6px'>{} · {}</div>{}</div>",
            escape_html(who),
            escape_html(when),
            if *is_html { body.clone() } else { format!("<pre>{}</pre>", escape_html(body)) }
        ));
    }
    wrap_body(true, &content, dark)
}

/// The content ids a body refers to, without their angle brackets.
pub fn cid_references(html: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let lower = html.to_lowercase();
    let bytes: Vec<char> = html.chars().collect();
    let lower: Vec<char> = lower.chars().collect();
    let needle: Vec<char> = "cid:".chars().collect();
    let mut i = 0;
    while i + needle.len() < lower.len() {
        if lower[i..i + needle.len()] == needle[..] {
            let mut j = i + needle.len();
            let mut id = String::new();
            while j < bytes.len() {
                let c = bytes[j];
                // The reference ends at the quote or delimiter around it.
                if c == '"' || c == '\'' || c == '>' || c == ' ' || c == ')' {
                    break;
                }
                id.push(c);
                j += 1;
            }
            let id = id.trim_matches(|c| c == '<' || c == '>').to_string();
            if !id.is_empty() {
                out.insert(id);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Replace `cid:` image references with the bytes they refer to.
///
/// Content ids appear in mail both bare and wrapped in angle brackets, and
/// the reference in the HTML may be either form, so both are matched.
/// The original message, quoted the way Outlook quotes it in a reply or
/// a forward, so what is on screen is what the recipient will read.
pub fn quoted_original(
    forwarded: bool,
    from: &str,
    sent: &str,
    to: &str,
    cc: &str,
    subject: &str,
    body: &str,
) -> String {
    let mut out = String::new();
    out.push_str(if forwarded {
        "\n\n---------- Forwarded message ----------\n"
    } else {
        "\n\n________________________________\n"
    });
    out.push_str(&format!("From: {from}\n"));
    if !sent.is_empty() {
        out.push_str(&format!("Sent: {sent}\n"));
    }
    if !to.is_empty() {
        out.push_str(&format!("To: {to}\n"));
    }
    if !cc.is_empty() {
        out.push_str(&format!("Cc: {cc}\n"));
    }
    out.push_str(&format!("Subject: {subject}\n\n"));
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

/// Typed text as a mail body. Plain text, so it is escaped rather than
/// interpreted, and its line breaks are kept by the layout rather than by
/// sprinkling tags through it.
pub fn text_as_html(text: &str) -> String {
    format!(
        "<div style=\"font-family:Segoe UI,Arial,sans-serif;font-size:11pt;\
         white-space:pre-wrap\">{}</div>",
        escape_html(text)
    )
}

/// A short, stable, filesystem-safe stand-in for a long opaque id.
/// Graph attachment ids are hundreds of characters, which makes an ugly
/// directory name and can overrun path limits; this keeps them apart
/// without carrying the whole thing around. FNV-1a, so the same id maps
/// to the same directory in every version.
pub fn short_key(id: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A name safe to use as a file name: no separators, no traversal, and
/// short enough for any filesystem. Attachment names come from mail, so
/// they are attacker-controlled.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || " .-_()[]".contains(c) { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    let cleaned = if cleaned.is_empty() { "attachment".to_string() } else { cleaned };
    cleaned.chars().take(120).collect()
}

pub fn inline_cid_images(html: &str, images: &[(String, String, String)]) -> String {
    let mut out = html.to_string();
    for (content_id, content_type, base64) in images {
        let bare = content_id.trim_matches(|c| c == '<' || c == '>');
        if bare.is_empty() {
            continue;
        }
        let data_uri = format!("data:{content_type};base64,{base64}");
        for reference in [format!("cid:{bare}"), format!("cid:<{bare}>")] {
            // Case-insensitive replace, since mail clients vary.
            let mut rebuilt = String::with_capacity(out.len());
            let mut rest = out.as_str();
            let needle = reference.to_lowercase();
            loop {
                let lower = rest.to_lowercase();
                match lower.find(&needle) {
                    Some(at) => {
                        rebuilt.push_str(&rest[..at]);
                        rebuilt.push_str(&data_uri);
                        rest = &rest[at + reference.len()..];
                    }
                    None => {
                        rebuilt.push_str(rest);
                        break;
                    }
                }
            }
            out = rebuilt;
        }
    }
    out
}

#[cfg(test)]
mod cid_tests {
    use super::*;

    fn image() -> (String, String, String) {
        ("logo@example".into(), "image/png".into(), "AAAA".into())
    }

    #[test]
    fn a_reference_becomes_the_image_itself() {
        let html = r#"<p>hi</p><img src="cid:logo@example" width="10">"#;
        let out = inline_cid_images(html, &[image()]);
        assert!(out.contains("data:image/png;base64,AAAA"));
        assert!(!out.contains("cid:"), "no dangling reference is left");
        assert!(out.contains("width=\"10\""), "the rest of the tag survives");
    }

    #[test]
    fn angle_brackets_and_case_are_tolerated() {
        let html = r#"<img src="CID:<logo@example>">"#;
        let out = inline_cid_images(html, &[image()]);
        assert!(out.contains("data:image/png;base64,AAAA"));
        assert!(!out.to_lowercase().contains("cid:"));
    }

    #[test]
    fn references_are_found_whatever_their_wrapping() {
        let html = r#"<img src="cid:a@x"><img src='CID:<b@y>'><img src="cid:c@z" alt="">"#;
        let found = cid_references(html);
        assert!(found.contains("a@x"), "plain reference");
        assert!(found.contains("b@y"), "angle brackets stripped");
        assert!(found.contains("c@z"), "stops at the quote");
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn a_body_without_pictures_asks_for_nothing() {
        assert!(cid_references("<p>no pictures here</p>").is_empty());
    }

    #[test]
    fn an_image_that_was_not_fetched_is_left_alone() {
        let html = r#"<img src="cid:missing@example">"#;
        assert_eq!(inline_cid_images(html, &[image()]), html);
    }
}

#[cfg(test)]
mod quote_tests {
    use super::*;

    #[test]
    fn a_forward_carries_the_original_and_says_who_sent_it() {
        let quote = quoted_original(
            true,
            "Mark de Jong <mark@fabrikam.nl>",
            "Thursday 10 September 2026, 09:10",
            "Vincent de Vries",
            "",
            "Licence renewal",
            "Procurement approved the renewal.",
        );
        assert!(quote.contains("Forwarded message"));
        assert!(quote.contains("From: Mark de Jong <mark@fabrikam.nl>"));
        assert!(quote.contains("Subject: Licence renewal"));
        assert!(quote.contains("Procurement approved the renewal."));
        // Room to type above it, as in Outlook.
        assert!(quote.starts_with("\n\n"));
        // Nothing to say about a Cc nobody was on.
        assert!(!quote.contains("Cc:"));
    }

    #[test]
    fn a_reply_quotes_without_calling_itself_a_forward() {
        let quote = quoted_original(false, "A <a@b.c>", "", "", "", "Hello", "Body");
        assert!(!quote.contains("Forwarded"));
        assert!(quote.contains("From: A <a@b.c>"));
        // An unknown date is left out rather than shown blank.
        assert!(!quote.contains("Sent:"));
    }

    /// What is typed is mail, not markup: a body mentioning a tag must
    /// arrive as that text rather than as an element.
    #[test]
    fn typed_text_is_escaped_not_interpreted() {
        let html = text_as_html("use <b> for bold\nsecond line");
        assert!(html.contains("&lt;b&gt;"));
        assert!(!html.contains("<b>"));
        // Line breaks survive without tags sprinkled through the text.
        assert!(html.contains("second line"));
        assert!(html.contains("pre-wrap"));
    }
}
