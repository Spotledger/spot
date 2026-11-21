//! Input limits: the nesting-depth pre-scan.
//!
//! A linear byte scan (no regex, no recursion) counts bracket nesting while
//! skipping strings, template literals and comments, so hostile input is
//! rejected before the parser sees it instead of hanging it.

use oxc_span::Span;

/// Contexts the scan tracks. A template literal acts as its own context:
/// `` `..${expr}..` `` — inside `${ }` the scanner is back in code (nested
/// templates nest like that), anywhere else backticks are literal text.
#[derive(Debug, Clone, Copy)]
enum Frame {
    /// Code at template depth zero (the top of the file, or a `${..}` body).
    Code,
    /// Text inside a template literal, waiting for a backtick or `${`.
    Template { braces: u32 },
}

/// Returns the span of the bracket whose opening would push nesting past
/// `cap`, or `None` when nothing up to that depth appears.
///
/// Unbalanced input is *not* an error here — the parser reports it properly.
pub fn over_nesting_cap(source: &str, cap: u32) -> Option<Span> {
    let bytes = source.as_bytes();
    let mut depth: u32 = 0;
    // Template frames stack: a backtick in code opens one; a `{` in a
    // template's `${` body opens another; a backtick in template text closes.
    let mut frames: Vec<Frame> = vec![Frame::Code];
    let in_code = |frames: &[Frame]| -> (bool, u32) {
        match frames.last() {
            Some(Frame::Code) => (true, 0),
            Some(Frame::Template { braces }) => (*braces > 0, *braces),
            None => (true, 0),
        }
    };
    let mut index = 0_usize;
    while index < bytes.len() {
        let byte = bytes[index];
        // Comments only exist in code contexts, never in template/string text.
        let (code_context, braces) = in_code(&frames);
        match byte {
            b'/' if code_context && index + 1 < bytes.len() && bytes[index + 1] == b'/' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'/' if code_context && index + 1 < bytes.len() && bytes[index + 1] == b'*' => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
                continue;
            }
            b'\\' if !matches!(frames.last(), Some(Frame::Code) if braces == 0) => {
                // Any escape (string, template) skips the next byte: `\"`,
                // `\``, `\${` are content, never structure.
                index += 2;
                continue;
            }
            b'\'' | b'"' if code_context => {
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index += 2,
                        _ if bytes[index] == byte => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                continue;
            }
            b'`' if braces == 0 && !code_context => {
                // Leaving template text.
                frames.pop();
                index += 1;
                continue;
            }
            b'`' if code_context => {
                // Entering template text (a template in expression position).
                frames.push(Frame::Template { braces: 0 });
                index += 1;
                continue;
            }
            b'$' if !code_context && index + 1 < bytes.len() && bytes[index + 1] == b'{' => {
                // Entering the `${ ... }` body: expression scanning resumes.
                if let Some(Frame::Template { braces: at }) = frames.last_mut() {
                    *at += 1;
                }
                depth += 1;
                if depth > cap {
                    return Some(Span::sized(index as u32, 2));
                }
                index += 2;
                continue;
            }
            b'{' | b'(' | b'[' if code_context => {
                depth += 1;
                if depth > cap {
                    return Some(Span::sized(index as u32, 1));
                }
            }
            b'}' if code_context && braces > 0 => {
                // The `}` that closes this template's `${..}` body.
                depth = depth.saturating_sub(1);
                if let Some(Frame::Template { braces: at }) = frames.last_mut() {
                    *at -= 1;
                }
            }
            b'}' | b')' | b']' if code_context => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        index += 1;
    }
    None
}
