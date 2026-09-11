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
/// Put what was typed above the message being answered or forwarded,
/// leaving that message's own markup untouched so it reaches the
/// recipient looking as it did — pictures, tables and all. The original
/// is a whole HTML document, so the text goes just inside its body

/// The date the way Outlook writes it in a quoted header:
/// "Monday, October 1, 2007 3:17 PM".
pub fn fmt_outlook_time(iso: &str) -> String {
    match DateTime::parse_from_rfc3339(iso) {
        Ok(t) => t.with_timezone(&Local).format("%A, %B %-d, %Y %-I:%M %p").to_string(),
        Err(_) => iso.to_string(),
    }
}

/// The header Outlook puts above a quoted message. It goes into the body
/// being edited, so it is HTML — plain, so it reads the same everywhere.
pub fn original_message_block(
    from: &str,
    sent: &str,
    to: &str,
    cc: &str,
    subject: &str,
) -> String {
    let mut lines = format!("From: {}", escape_html(from));
    if !sent.is_empty() {
        lines.push_str(&format!("<br>Sent: {}", escape_html(sent)));
    }
    if !to.is_empty() {
        lines.push_str(&format!("<br>To: {}", escape_html(to)));
    }
    if !cc.is_empty() {
        lines.push_str(&format!("<br>Cc: {}", escape_html(cc)));
    }
    lines.push_str(&format!("<br>Subject: {}", escape_html(subject)));
    format!(
        "<div style=\"font-family:'Segoe UI',Ubuntu,sans-serif;font-size:11pt\">\
         <b>-----Original Message-----</b><br>{lines}</div><div><br></div>"
    )
}

/// Mail being edited is still mail: untrusted. The composer has to run
/// scripting to read back what was written, so anything that could run on
/// its own is taken out of the message first.
pub fn sanitize_for_editor(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    // Drop <script>/<style-with-script> elements whole, contents and all.
    loop {
        let lower = rest.to_lowercase();
        let Some(open) = lower.find("<script") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        rest = match lower[open..].find("</script>") {
            Some(close) => &rest[open + close + "</script>".len()..],
            // Unclosed: the remainder is all script, so none of it survives.
            None => "",
        };
    }
    strip_event_handlers(&out)
}

/// Remove `on…="…"` attributes and `javascript:` targets, wherever they sit.
fn strip_event_handlers(html: &str) -> String {
    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let is_handler = lower[i..].starts_with(" on")
            && lower[i + 3..].chars().next().is_some_and(|c| c.is_ascii_alphabetic());
        if is_handler {
            // Skip the whole attribute, quoted value included.
            let mut j = i + 3;
            while j < bytes.len() && bytes[j] != b'=' && bytes[j] != b'>' && bytes[j] != b' ' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && bytes[j] == b' ' {
                    j += 1;
                }
                match bytes.get(j) {
                    Some(&q @ (b'"' | b'\'')) => {
                        j += 1;
                        while j < bytes.len() && bytes[j] != q {
                            j += 1;
                        }
                        j = (j + 1).min(bytes.len());
                    }
                    _ => {
                        while j < bytes.len() && bytes[j] != b' ' && bytes[j] != b'>' {
                            j += 1;
                        }
                    }
                }
                i = j;
                continue;
            }
        }
        if lower[i..].starts_with("javascript:") {
            out.push_str("about:blank");
            i += "javascript:".len();
            continue;
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The composer document: what is being written, edited in place. The
/// message being answered sits in the same body, below the header block,
/// so trimming it works like anywhere else.
pub fn editor_document(dark: bool, quoted: &str) -> String {
    let (fg, bg, quote, link) = if dark {
        ("#e3e3e3", "#1e1e1e", "#9a9a9a", "#7cb7f2")
    } else {
        ("#1a1a1a", "#ffffff", "#555555", "#0f6cbd")
    };
    format!(
        "<!doctype html><html><head><meta charset='utf-8'>\
         <meta name='viewport' content='width=device-width, initial-scale=1'><style>\
         body{{font-family:'Segoe UI',Ubuntu,Cantarell,sans-serif;font-size:14px;\
         line-height:1.5;color:{fg};background:{bg};margin:12px;\
         overflow-wrap:break-word;}}\
         body:focus{{outline:none;}}\
         pre{{white-space:pre-wrap;font:inherit;}}\
         img{{max-width:100%;height:auto;}}\
         table{{max-width:100%;}}\
         blockquote{{border-left:3px solid {quote};margin-left:0;padding-left:12px;color:{quote};}}\
         a{{color:{link};}}\
         </style></head><body contenteditable='true'><div><br></div><div><br></div>\
         {quoted}</body></html>"
    )
}

/// The original message, quoted the way Outlook quotes it in a reply or

/// Typed text as a mail body. Plain text, so it is escaped rather than
/// interpreted, and its line breaks are kept by the layout rather than by

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
mod compose_tests {
    use super::*;

    #[test]
    fn the_header_reads_the_way_outlook_writes_it() {
        let block = original_message_block(
            "Mark de Jong <mark@fabrikam.nl>",
            "Monday, October 1, 2007 3:17 PM",
            "Vincent de Vries",
            "",
            "Licence renewal",
        );
        assert!(block.contains("-----Original Message-----"));
        assert!(block.contains("From: Mark de Jong &lt;mark@fabrikam.nl&gt;"));
        assert!(block.contains("Sent: Monday, October 1, 2007 3:17 PM"));
        assert!(block.contains("To: Vincent de Vries"));
        assert!(block.contains("Subject: Licence renewal"));
        // Nothing to say about a Cc nobody was on.
        assert!(!block.contains("Cc:"));
    }

    /// The composer runs scripting to read back what was written, so a
    /// message must not be able to bring scripting of its own.
    #[test]
    fn a_quoted_message_cannot_run_anything() {
        let hostile = "<p>Hello</p><script>steal()</script>\
                       <img src=x onerror=\"steal()\"><a href='javascript:steal()'>click</a>";
        let safe = sanitize_for_editor(hostile);
        assert!(safe.contains("Hello"), "the message itself survives");
        assert!(!safe.to_lowercase().contains("<script"));
        assert!(!safe.to_lowercase().contains("onerror"));
        assert!(!safe.to_lowercase().contains("javascript:"));
        assert!(safe.contains("<img"), "the picture is kept, only its handler goes");
    }

    #[test]
    fn an_unclosed_script_takes_nothing_with_it() {
        let safe = sanitize_for_editor("<p>Keep</p><script>never();");
        assert!(safe.contains("Keep"));
        assert!(!safe.contains("never()"));
    }

    #[test]
    fn formatting_and_pictures_are_left_alone() {
        let mail = "<table><tr><td style='color:red'>Cell</td></tr></table>\
                    <img src='data:image/png;base64,AAA'>";
        assert_eq!(sanitize_for_editor(mail), mail);
    }

    /// The message is written above the quote, so that is where the
    /// document opens.
    #[test]
    fn the_editor_opens_with_room_above_the_quote() {
        let doc = editor_document(false, "<div>-----Original Message-----</div>");
        assert!(doc.contains("contenteditable='true'"));
        let typing_space = doc.find("<div><br></div>").expect("space to type in");
        let quote = doc.find("-----Original Message-----").expect("the quote");
        assert!(typing_space < quote, "the blank line comes first");
    }
}
