//! WL-034: pure render layer for the static, offline mailbox export.
//!
//! Everything here is **I/O-free and pure**: it receives messages already fetched
//! through the `Store` trait (the `weave export` CLI handler does the read + the
//! file write) and turns them into one self-contained HTML document. That purity
//! is what makes the bundle unit-testable with a `Vec<Message>` and no DB, and it
//! keeps the layer DAG intact (this module reads no DB and opens no socket).
//!
//! ## XSS — the single biggest correctness risk
//!
//! This module owns the **one** [`html_escape`] helper for the whole workspace
//! (the surfaces dashboard re-uses it). Two independent barriers protect the
//! rendered bundle:
//!
//! 1. **Static HTML cells** (the `<noscript>` fallback table) interpolate every
//!    `Message`-derived string through [`html_escape`] — never `format!("…{body}…")`
//!    of raw text.
//! 2. **The inlined JSON data block** is serialized with `serde_json` and then made
//!    script-safe: the byte sequence `</` is rewritten to `<\/` so a body containing
//!    a literal `</script>` cannot terminate the `<script type="application/json">`
//!    block. `<\/` is a valid JSON string escape, so `JSON.parse` still succeeds.
//!    The client JS renders rows with `textContent` (never `innerHTML`), a second,
//!    independent XSS barrier.

use crate::model::{fmt_ts, Message};

/// Escape the five HTML-significant characters so caller-derived text can never
/// break out of an element body or attribute. This is the central XSS defense:
/// `&` first (so we don't double-escape the entities we emit), then `<`, `>`,
/// `"`, `'`.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// Make a serialized-JSON string safe to embed inside a `<script>` element.
///
/// The HTML tokenizer ends a script element at the literal byte sequence
/// `</script` (case-insensitive), regardless of the `type` attribute. Rewriting
/// every `</` to `<\/` neutralizes that without changing the decoded value:
/// `\/` is a legal JSON escape for `/`, so `JSON.parse` of the embedded text
/// yields the identical string. We also defang `<!--` (HTML comment open) which
/// can confuse the "script data" tokenizer state.
fn script_safe_json(json: &str) -> String {
    json.replace("</", "<\\/").replace("<!--", "<\\!--")
}

/// Pure: render a self-contained, offline-openable HTML mailbox bundle.
///
/// - No I/O, no `Store`, no socket — unit-testable with a `Vec<Message>`.
/// - Inlines the messages as JSON in a `<script type="application/json">` block,
///   `</script>`-neutralized via [`script_safe_json`].
/// - Embeds an inline `<style>` and vanilla-JS client-side search/filter — no
///   external `<script src>` / `<link href>` / CDN.
/// - The client renders rows via `textContent`; a `<noscript>` fallback table
///   renders every field through [`html_escape`].
pub fn render_mailbox_html(messages: &[Message]) -> String {
    let json = serde_json::to_string(messages).unwrap_or_else(|_| "[]".to_string());
    let data = script_safe_json(&json);

    let mut b = String::with_capacity(4096 + data.len());
    b.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    b.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    b.push_str("<title>weave mailbox export</title>");
    b.push_str("<style>");
    b.push_str(
        "body{font-family:system-ui,sans-serif;margin:1.5rem;background:#0d1117;color:#c9d1d9}\
         h1{font-size:1.3rem}\
         #q{width:100%;max-width:32rem;padding:.4rem .6rem;margin:.5rem 0 1rem;\
         background:#161b22;color:#c9d1d9;border:1px solid #30363d;border-radius:6px;font-size:.95rem}\
         table{border-collapse:collapse;width:100%;font-size:.85rem}\
         td,th{text-align:left;padding:.25rem .5rem;border-bottom:1px solid #21262d;vertical-align:top}\
         th{color:#8b949e}\
         .empty{color:#8b949e;font-style:italic}\
         code{color:#79c0ff}\
         .body{white-space:pre-wrap;word-break:break-word}",
    );
    b.push_str("</style></head><body>");
    b.push_str("<h1>weave mailbox export</h1>");
    b.push_str("<input id=\"q\" type=\"search\" placeholder=\"search sender / recipient / subject / body…\" autocomplete=\"off\">");
    b.push_str("<p class=\"empty\" id=\"count\"></p>");
    b.push_str("<table><thead><tr><th>ts</th><th>from</th><th>to</th><th>subject</th><th>body</th></tr></thead>");
    b.push_str("<tbody id=\"rows\"></tbody></table>");

    // Non-JS fallback: a server-rendered table, every field through html_escape.
    b.push_str("<noscript><table><thead><tr><th>ts</th><th>from</th><th>to</th>");
    b.push_str("<th>subject</th><th>body</th></tr></thead><tbody>");
    if messages.is_empty() {
        b.push_str("<tr><td colspan=\"5\" class=\"empty\">no messages</td></tr>");
    } else {
        for m in messages {
            b.push_str("<tr><td>");
            b.push_str(&html_escape(&fmt_ts(m.ts)));
            b.push_str("</td><td>");
            b.push_str(&html_escape(&m.sender));
            b.push_str("</td><td>");
            b.push_str(&html_escape(&m.recipient));
            b.push_str("</td><td>");
            b.push_str(&html_escape(m.subject.as_deref().unwrap_or("")));
            b.push_str("</td><td class=\"body\">");
            b.push_str(&html_escape(&m.body));
            b.push_str("</td></tr>");
        }
    }
    b.push_str("</tbody></table></noscript>");

    // The data block: NOT executed, NOT a <script src>; read via textContent.
    b.push_str("<script type=\"application/json\" id=\"weave-data\">");
    b.push_str(&data);
    b.push_str("</script>");

    // Vanilla-JS client: parse the data, filter by substring, render via
    // textContent / createElement so message text is inserted as text nodes.
    b.push_str(
        "<script>(function(){\
         var raw=document.getElementById('weave-data').textContent;\
         var msgs;try{msgs=JSON.parse(raw)}catch(e){msgs=[]}\
         var tbody=document.getElementById('rows');\
         var count=document.getElementById('count');\
         var q=document.getElementById('q');\
         function fmt(ts){return new Date(ts*1000).toISOString().replace('T',' ').slice(0,19)}\
         function cell(text,cls){var td=document.createElement('td');\
         if(cls){td.className=cls}td.textContent=text==null?'':String(text);return td}\
         function render(filter){\
         tbody.textContent='';\
         var f=(filter||'').toLowerCase();var n=0;\
         for(var i=0;i<msgs.length;i++){var m=msgs[i];\
         var hay=((m.sender||'')+' '+(m.recipient||'')+' '+(m.subject||'')+' '+(m.body||'')).toLowerCase();\
         if(f&&hay.indexOf(f)===-1){continue}\
         n++;var tr=document.createElement('tr');\
         tr.appendChild(cell(fmt(m.ts)));\
         tr.appendChild(cell(m.sender));\
         tr.appendChild(cell(m.recipient));\
         tr.appendChild(cell(m.subject));\
         tr.appendChild(cell(m.body,'body'));\
         tbody.appendChild(tr)}\
         count.textContent=msgs.length===0?'no messages':(n+' of '+msgs.length+' messages');\
         }\
         q.addEventListener('input',function(){render(q.value)});\
         render('');\
         })();</script>",
    );

    b.push_str("</body></html>");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: i64, sender: &str, recipient: &str, subject: Option<&str>, body: &str) -> Message {
        Message {
            id,
            ts: 1000,
            sender: sender.to_string(),
            recipient: recipient.to_string(),
            subject: subject.map(|s| s.to_string()),
            body: body.to_string(),
            in_reply_to: None,
            idempotency_key: None,
            trace_id: None,
            priority: "normal".to_string(),
            superseded_by: None,
        }
    }

    #[test]
    fn html_escape_basics() {
        assert_eq!(html_escape("a<b>c"), "a&lt;b&gt;c");
        assert_eq!(html_escape("x&y"), "x&amp;y");
        assert_eq!(html_escape("\"q\" 'p'"), "&quot;q&quot; &#x27;p&#x27;");
        assert_eq!(html_escape("hello world 123"), "hello world 123");
    }

    #[test]
    fn render_is_self_contained() {
        let html = render_mailbox_html(&[msg(1, "a", "b", None, "hi")]);
        assert!(
            !html.contains("<script src"),
            "must not reference external scripts"
        );
        assert!(
            !html.contains("<link "),
            "must not reference external stylesheets"
        );
        assert!(!html.contains("http://") && !html.contains("https://"));
        assert!(html.contains("<script type=\"application/json\" id=\"weave-data\">"));
    }

    #[test]
    fn render_escapes_static_fields() {
        let html =
            render_mailbox_html(&[msg(1, "<b>", "bob", None, "<img src=x onerror=alert(1)>")]);
        // The <noscript> static region escapes the payload.
        assert!(html.contains("&lt;b&gt;"), "sender must be escaped");
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        // Scope the "no raw tag" assertion to the static <noscript> region. The
        // raw payload DOES legitimately appear inside the `<script
        // type="application/json">` data block (serde_json does not escape `<`/`>`,
        // and that block is inert — not HTML-parsed, read via textContent); only a
        // literal `</script` could terminate it, and `</` is neutralized elsewhere.
        // The invariant under test is that the *server-rendered* fallback cells —
        // the only place message text becomes live markup with JS off — are escaped.
        let nstart = html.find("<noscript>").expect("noscript region present");
        let nend = html[nstart..]
            .find("</noscript>")
            .expect("noscript region closed")
            + nstart;
        let noscript = &html[nstart..nend];
        assert!(
            !noscript.contains("<img src=x onerror=alert(1)>"),
            "raw payload must not survive un-escaped in the static <noscript> region"
        );
    }

    #[test]
    fn render_neutralizes_script_close_in_json() {
        let payload = "</script><script>alert(1)</script>";
        let html = render_mailbox_html(&[msg(1, "evil", "bob", None, payload)]);
        // Extract the application/json data block.
        let start = html
            .find("id=\"weave-data\">")
            .map(|i| i + "id=\"weave-data\">".len())
            .expect("data block start");
        let end = html[start..].find("</script>").expect("data block end") + start;
        let data = &html[start..end];
        // No raw `</` survives inside the data block (the script-close case).
        assert!(
            !data.contains("</"),
            "raw </ must be neutralized in data block: {data}"
        );
        assert!(data.contains("<\\/"), "expected escaped <\\/ in data block");
        // Round-trip: the neutralized JSON still parses back to the messages,
        // after undoing the cosmetic script-safe escaping.
        let restored = data.replace("<\\/", "</").replace("<\\!--", "<!--");
        let parsed: Vec<Message> = serde_json::from_str(&restored).expect("data block parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].body, payload);
    }

    #[test]
    fn render_empty_mailbox() {
        let html = render_mailbox_html(&[]);
        assert!(html.contains("no messages"));
        assert!(html.contains("<script type=\"application/json\" id=\"weave-data\">[]</script>"));
    }
}
