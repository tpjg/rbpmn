//! Document assembly: one bundle, one stylesheet, optionally one data block,
//! and a policy the document carries itself.
//!
//! The split that makes the policy a constant: **executable** JavaScript is a
//! single inline `<script>` whose SHA-256 is fixed at build time, and the
//! per-instance data goes in `<script type="application/json">`, which is not
//! executable and cannot be promoted to script. Both hashes are listed —
//! browsers disagree about whether `script-src` governs non-executable data
//! blocks, and listing one extra hash costs nothing while guessing wrong
//! costs a blank page.

use sha2::{Digest, Sha256};

/// `connect-src 'none'` is the guarantee worth stating: no fetch, no XHR, no
/// WebSocket, no beacon. Together with every other fetch directive pinned to
/// `'none'` or `data:`, a document containing business data cannot phone
/// home.
///
/// `style-src 'unsafe-inline'` is deliberate and does not weaken that.
/// diagram-js styles elements at runtime, and CSS is not an exfiltration
/// channel here: `url()` loads resolve under `img-src`/`font-src`, both of
/// which are `data:`-only, so injected CSS has nowhere to send anything.
/// Script execution — the thing that would matter — stays hash-pinned.
fn policy(script_hashes: &[String], wasm: bool) -> String {
    let mut script_src = script_hashes
        .iter()
        .map(|h| format!("'sha256-{h}'"))
        .collect::<Vec<_>>()
        .join(" ");
    if wasm {
        // CSP3 requires this for WebAssembly compilation. The inspector does
        // not carry a validator and so does not get it.
        script_src.push_str(" 'wasm-unsafe-eval'");
    }
    format!(
        "default-src 'none'; script-src {script_src}; style-src 'unsafe-inline'; \
         img-src data:; font-src data:; connect-src 'none'; base-uri 'none'; \
         form-action 'none'"
    )
}

pub(crate) fn sha256_base64(content: &str) -> String {
    base64(&Sha256::digest(content.as_bytes()))
}

/// Standard base64. Small enough to own outright rather than take a
/// dependency for one call site.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = u32::from(chunk[0]);
        let b2 = chunk.get(1).copied().map_or(0, u32::from);
        let b3 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Escapes text destined for HTML *character data*. Used for the title —
/// everything else the documents render, they render through the DOM with
/// `textContent`.
pub(crate) fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes a value going into a double-quoted attribute.
///
/// Deliberately leaves `'` alone. A policy is full of single quotes
/// (`'none'`, `'sha256-…'`); a browser would decode `&#39;` back to `'` and
/// enforce the same thing either way, but a security header nobody can read
/// in view-source is one nobody will audit.
pub(crate) fn escape_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes serialized JSON for embedding in an HTML `<script>` data block.
///
/// In JSON, `<`, `>` and `&` can only ever occur *inside* string literals —
/// never in structural syntax — so replacing them with their `\uXXXX` escapes
/// preserves the value exactly while making `</script`, `<!--` and `<![CDATA[`
/// unrepresentable. U+2028/U+2029 are escaped too: they are legal in a JSON
/// string but terminate a line in JavaScript source, and this text is about
/// to sit inside a script element.
///
/// This is the whole defence for business data — order notes, failure
/// messages, variable values — that no one on this side of the boundary
/// controls. It is not sanitization (nothing is removed, and nothing should
/// be); it is escaping, and it has to be exactly right.
pub(crate) fn escape_json_for_html(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(c),
        }
    }
    out
}

pub(crate) struct Document<'a> {
    pub title: &'a str,
    pub css: &'a str,
    pub js: &'a str,
    /// Serialized JSON, already escaped by [`escape_json_for_html`].
    pub data: Option<String>,
    pub wasm: bool,
}

impl Document<'_> {
    pub fn render(&self) -> String {
        let mut hashes = vec![sha256_base64(self.js)];
        if let Some(data) = &self.data {
            hashes.push(sha256_base64(data));
        }
        let csp = policy(&hashes, self.wasm);

        let mut out = String::with_capacity(self.js.len() + self.css.len() + 4096);
        out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
        out.push_str("<meta charset=\"utf-8\">\n");
        out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
        out.push_str(&format!(
            "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">\n",
            escape_attribute(&csp)
        ));
        out.push_str(&format!("<title>{}</title>\n", escape_html(self.title)));
        out.push_str("<style>\n");
        out.push_str(self.css);
        out.push_str("\n</style>\n</head>\n<body>\n");
        out.push_str("<div id=\"rbpmn-root\"></div>\n");
        out.push_str(
            "<noscript>This document renders a BPMN diagram and needs JavaScript. \
             Nothing is loaded from the network — the script is part of the file.</noscript>\n",
        );
        if let Some(data) = &self.data {
            out.push_str("<script type=\"application/json\" id=\"rbpmn-data\">");
            out.push_str(data);
            out.push_str("</script>\n");
        }
        out.push_str("<script>");
        out.push_str(self.js);
        out.push_str("</script>\n</body>\n</html>\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // Bytes above 0x7f must not go through a char conversion.
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn sha256_matches_a_known_digest() {
        // The empty string's SHA-256, base64-encoded — the value a browser
        // compares against for an empty inline script.
        assert_eq!(
            sha256_base64(""),
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
    }

    #[test]
    fn json_escaping_makes_a_script_close_unrepresentable() {
        let hostile = serde_json::json!({ "note": "</script><img src=x onerror=alert(1)>" });
        let escaped = escape_json_for_html(&hostile.to_string());
        assert!(!escaped.contains("</script"));
        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
        // ... and it still parses back to exactly the original value.
        let back: serde_json::Value = serde_json::from_str(&escaped).unwrap();
        assert_eq!(back, hostile);
    }

    #[test]
    fn json_escaping_survives_line_separators() {
        let hostile = serde_json::json!({ "note": "a\u{2028}b\u{2029}c" });
        let escaped = escape_json_for_html(&hostile.to_string());
        assert!(!escaped.contains('\u{2028}'));
        assert!(!escaped.contains('\u{2029}'));
        let back: serde_json::Value = serde_json::from_str(&escaped).unwrap();
        assert_eq!(back, hostile);
    }
}
