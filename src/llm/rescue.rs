//! Reading a tool call the model wrote as text.
//!
//! Small models drop out of structured tool calling under load and write the
//! call into their prose instead. The intention is well-formed; only the
//! channel is wrong. Worksmith used to see no `tool_calls`, score the reply
//! empty, nudge, and get the same thing back — one whole turn spent refusing to
//! read what the model plainly said.
//!
//! Two things about *where* the text lands, both learned the hard way:
//!
//! 1. **Usually it is `reasoning`, not `content`.** That is the entire
//!    explanation for "the model returned an empty response": `content`
//!    genuinely is empty and the call went into the provider's reasoning field,
//!    which is display-only. A parser reading only `content` would never fire.
//!
//! 2. **The `<tool_call>` wrapper is already gone from `content`** by the time
//!    it gets here — `strip_toolcall_noise` removes it upstream, and has to,
//!    because providers leak fragments of it into ordinary text. So nothing may
//!    depend on that wrapper being present. The shapes below anchor on
//!    `<function=` and on the JSON object itself, both of which survive.
//!
//! The one rule that makes this safe: it only ever runs when the structured
//! `tool_calls` field is empty. A model that produced both is already being
//! understood, and reinterpreting its prose would invent calls it did not make.

use super::{Completion, ToolCall, ToolDef};

/// Promote a tool call the model wrote as text, if there is one to promote.
///
/// Returns a note to show the user when something was promoted, and `None` when
/// the completion was left exactly as it arrived — which is the overwhelmingly
/// common case.
pub(crate) fn rescue_text_tool_calls(
    completion: &mut Completion,
    tools: &[ToolDef],
) -> Option<String> {
    // Stated first because it is the guard everything else rests on.
    if !completion.tool_calls.is_empty() {
        return None;
    }

    // Content first: if the model put the call where text goes, that is what it
    // meant. Only when content yields nothing do we read the reasoning, which is
    // where these actually land most of the time.
    for field in [Field::Content, Field::Reasoning] {
        let text = match field {
            Field::Content => completion.content.as_deref(),
            Field::Reasoning => completion.reasoning.as_deref(),
        };
        let Some(text) = text else { continue };
        let (mut calls, kept) = extract(text, tools);
        if calls.is_empty() {
            continue;
        }

        for (i, call) in calls.iter_mut().enumerate() {
            call.id = format!("text_call_{i}");
        }
        let named: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
        let what = if named.len() == 1 {
            format!("its `{}` call", named[0])
        } else {
            format!("{} tool calls (`{}`)", named.len(), named.join("`, `"))
        };
        completion.tool_calls = calls;

        // Strip the block from wherever it came from, and keep the prose around
        // it — the sighting that prompted this had a sentence after the block.
        // Left in place, the text would be both spoken and executed.
        let kept = if kept.trim().is_empty() { None } else { Some(kept) };
        match field {
            Field::Content => completion.content = kept,
            Field::Reasoning => completion.reasoning = kept,
        }

        return Some(format!(
            "the model wrote {what} as text in `{}` instead of using the API's tool-call \
             field; worksmith read it and ran it anyway. Said once per session — a model \
             doing this is drifting out of structured tool calling, which is worth knowing \
             when choosing one.",
            field.name(),
        ));
    }
    None
}

#[derive(Clone, Copy)]
enum Field {
    Content,
    Reasoning,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Field::Content => "content",
            Field::Reasoning => "reasoning",
        }
    }
}

/// Pull every tool call out of one field, returning the calls and the text with
/// those blocks removed.
///
/// Deliberately literal. A loose parser here fabricates tool calls out of prose,
/// which is a worse failure than the one it fixes: anything that is not one of
/// the shapes below is left alone as text.
fn extract(text: &str, tools: &[ToolDef]) -> (Vec<ToolCall>, String) {
    let mut calls = Vec::new();
    let mut kept = String::new();
    let mut cursor = 0usize;

    loop {
        let xml = text[cursor..].find("<function=").map(|p| (cursor + p, Shape::Xml));
        let fence = text[cursor..].find("```").map(|p| (cursor + p, Shape::Fence));
        let (start, shape) = match (xml, fence) {
            (None, None) => break,
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (Some(a), Some(b)) => {
                if a.0 <= b.0 {
                    a
                } else {
                    b
                }
            }
        };

        let parsed = match shape {
            Shape::Xml => parse_xml(&text[start..], tools),
            Shape::Fence => parse_fenced(&text[start..], tools),
        };
        match parsed {
            Some((len, call)) => {
                kept.push_str(&text[cursor..start]);
                calls.push(call);
                cursor = start + len;
            }
            None => {
                // Not a call we understand. Keep it as prose, and step past the
                // opener so the scan cannot spin on the same position.
                let step = shape.opener_len();
                let next = (start + step).min(text.len());
                kept.push_str(&text[cursor..next]);
                cursor = next;
            }
        }
    }
    kept.push_str(&text[cursor..]);

    if calls.is_empty() {
        // The Hermes wrapper is stripped from `content` before it reaches here,
        // so the classic `<tool_call>{"name":…}</tool_call>` arrives as a bare
        // object. Accepted only when it is the *entire* field: a JSON object
        // sitting inside prose is far more likely to be the model showing its
        // work than asking for a tool.
        if let Some(call) = parse_call_object(text.trim(), tools) {
            return (vec![call], String::new());
        }
    }
    (calls, kept)
}

#[derive(Clone, Copy)]
enum Shape {
    /// `<function=NAME><parameter=KEY>VALUE</parameter>…</function>`
    Xml,
    /// A fenced JSON object naming a tool.
    Fence,
}

impl Shape {
    fn opener_len(self) -> usize {
        match self {
            Shape::Xml => "<function=".len(),
            Shape::Fence => "```".len(),
        }
    }
}

/// Parse `<function=NAME>…</function>` at the head of `s`, returning its byte
/// length and the call. The `<tool_call>` wrapper around it, when there is one,
/// is not required and not consumed — it is ordinary text either way.
fn parse_xml(s: &str, tools: &[ToolDef]) -> Option<(usize, ToolCall)> {
    let after = s.strip_prefix("<function=")?;
    let gt = after.find('>')?;
    let name = after[..gt].trim().trim_matches('"');
    // A promoted call to a tool that does not exist is a hallucination given
    // hands. The advertised list is the whole check.
    let def = tools.iter().find(|t| t.name == name)?;

    let close = after.find("</function>")?;
    if close < gt {
        return None;
    }
    let body = &after[gt + 1..close];
    // An unclosed block followed by a closed one would otherwise swallow the
    // second block's parameters into the first block's name.
    if body.contains("<function=") {
        return None;
    }

    let mut args = serde_json::Map::new();
    let mut cur = 0usize;
    while let Some(p) = body[cur..].find("<parameter=") {
        let key_start = cur + p + "<parameter=".len();
        let g = body[key_start..].find('>')?;
        let key = body[key_start..key_start + g].trim().trim_matches('"').to_string();
        let val_start = key_start + g + 1;
        // A missing close tag means the block was truncated. Leave the whole
        // thing as text: a tool call with half its arguments costs a turn just
        // as surely as an unread one, and this way the text is at least visible.
        let e = body[val_start..].find("</parameter>")?;
        let raw = trim_one_newline(&body[val_start..val_start + e]);
        args.insert(key.clone(), coerce(raw, &key, def));
        cur = val_start + e + "</parameter>".len();
    }

    let len = "<function=".len() + close + "</function>".len();
    Some((
        len,
        ToolCall {
            id: String::new(),
            name: name.to_string(),
            arguments: serde_json::Value::Object(args).to_string(),
        },
    ))
}

/// Parse a ```` ```json {…} ```` block at the head of `s`.
fn parse_fenced(s: &str, tools: &[ToolDef]) -> Option<(usize, ToolCall)> {
    let after = s.strip_prefix("```")?;
    let nl = after.find('\n')?;
    let tag = after[..nl].trim();
    if !tag.is_empty() && !tag.eq_ignore_ascii_case("json") {
        return None;
    }
    let body_start = nl + 1;
    let close = after[body_start..].find("```")?;
    let call = parse_call_object(after[body_start..body_start + close].trim(), tools)?;
    Some(("```".len() + body_start + close + "```".len(), call))
}

/// A JSON object that *is* a tool call: `{"name": "bash", "arguments": {…}}`.
///
/// Three conditions, and they matter together. The name must be advertised; the
/// arguments must parse; and there must be no other keys, so that an object the
/// model was merely discussing is not mistaken for a request to act.
fn parse_call_object(s: &str, tools: &[ToolDef]) -> Option<ToolCall> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let obj = v.as_object()?;
    if !obj.keys().all(|k| matches!(k.as_str(), "name" | "arguments" | "parameters")) {
        return None;
    }
    let name = obj.get("name")?.as_str()?;
    if !tools.iter().any(|t| t.name == name) {
        return None;
    }

    let raw = obj.get("arguments").or_else(|| obj.get("parameters"));
    let args = match raw {
        None => serde_json::Map::new(),
        Some(serde_json::Value::Object(m)) => m.clone(),
        // Some servers double-encode the arguments, exactly as the real
        // tool-call field does.
        Some(serde_json::Value::String(t)) => match serde_json::from_str(t) {
            Ok(serde_json::Value::Object(m)) => m,
            _ => return None,
        },
        Some(_) => return None,
    };

    Some(ToolCall {
        id: String::new(),
        name: name.to_string(),
        arguments: serde_json::Value::Object(args).to_string(),
    })
}

/// XML parameters carry no types, so a `limit` of `10` would arrive as the
/// string `"10"` and fail the tool's schema on arrival. The tool's own
/// advertised schema says what each key should be, so use it — and fall back to
/// a string whenever it does not say, which is the shape most arguments have.
fn coerce(raw: &str, key: &str, def: &ToolDef) -> serde_json::Value {
    let ty = def
        .parameters
        .get("properties")
        .and_then(|p| p.get(key))
        .and_then(|s| s.get("type"))
        .and_then(|t| t.as_str());
    let text = raw.trim();
    let parsed = match ty {
        Some("integer") | Some("number") => text.parse::<serde_json::Number>().ok().map(Into::into),
        Some("boolean") => text.parse::<bool>().ok().map(Into::into),
        Some("array") | Some("object") => serde_json::from_str(text).ok(),
        _ => None,
    };
    parsed.unwrap_or_else(|| serde_json::Value::String(raw.to_string()))
}

/// Strip the one newline the XML convention puts after the open tag and before
/// the close tag, and no more. Trimming further would eat the trailing newline
/// of a file being written, which is a real edit to somebody's content.
fn trim_one_newline(s: &str) -> &str {
    let s = s.strip_prefix("\r\n").or_else(|| s.strip_prefix('\n')).unwrap_or(s);
    s.strip_suffix("\r\n").or_else(|| s.strip_suffix('\n')).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Usage;

    /// The tools the parser is allowed to promote to, shaped like the real
    /// ones — `read` has a typed `limit`, which is what forces the coercion.
    fn tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "bash".into(),
                description: String::new(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } }
                }),
            },
            ToolDef {
                name: "read".into(),
                description: String::new(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "limit": { "type": "integer" }
                    }
                }),
            },
        ]
    }

    fn from_content(text: &str) -> Completion {
        Completion { content: Some(text.into()), ..Completion::default() }
    }

    fn from_reasoning(text: &str) -> Completion {
        Completion { reasoning: Some(text.into()), ..Completion::default() }
    }

    #[test]
    fn the_live_sighting_in_reasoning_is_promoted_and_the_prose_survives() {
        // Copied from the 2026-08-30 transcript, hosted qwen3.5-9b: the call
        // went into `reasoning`, `content` was empty, and a sentence followed
        // the block. Worksmith called the whole turn empty.
        let mut c = from_reasoning(
            "<tool_call>\n<function=bash>\n<parameter=command>\n\
             pip3 install pytest -q 2>&1 | tail -5\n\
             </parameter>\n</function>\n</tool_call>\
             The background worker task ran but I need to verify it.",
        );
        let note = rescue_text_tool_calls(&mut c, &tools()).expect("promoted");

        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "bash");
        assert_eq!(
            c.tool_calls[0].arguments,
            r#"{"command":"pip3 install pytest -q 2>&1 | tail -5"}"#
        );
        assert!(note.contains("reasoning"), "the note says where it came from: {note}");

        let left = c.reasoning.unwrap();
        assert!(left.contains("verify it."), "the sentence after the block is kept");
        assert!(!left.contains("<function="), "the block itself is gone: {left}");
    }

    #[test]
    fn content_works_even_though_the_wrapper_was_stripped_upstream() {
        // `strip_toolcall_noise` removes `<tool_call>` from content before it
        // ever reaches here, so this is what a content-channel call looks like.
        // Anchoring on the wrapper would have found nothing.
        let mut c = from_content(
            "Let me check.\n<function=read>\n<parameter=path>src/tui.rs</parameter>\n\
             <parameter=limit>40</parameter>\n</function>",
        );
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_some());
        assert_eq!(c.tool_calls[0].name, "read");
        // `limit` is an integer in the schema, so it must not arrive as "40".
        assert_eq!(c.tool_calls[0].arguments, r#"{"limit":40,"path":"src/tui.rs"}"#);
        assert_eq!(c.content.as_deref(), Some("Let me check.\n"));
    }

    #[test]
    fn a_fenced_json_object_is_promoted() {
        let mut c = from_content(
            "Running it:\n```json\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\n```\ndone",
        );
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_some());
        assert_eq!(c.tool_calls[0].name, "bash");
        assert_eq!(c.tool_calls[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(c.content.as_deref(), Some("Running it:\n\ndone"));
    }

    #[test]
    fn a_bare_hermes_object_alone_in_the_field_is_promoted() {
        // What `<tool_call>{…}</tool_call>` becomes once the wrapper is
        // stripped. Only accepted as the whole field.
        let mut c = from_content("{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}");
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_some());
        assert_eq!(c.tool_calls[0].name, "bash");
        assert_eq!(c.content, None, "nothing was left to also say out loud");
    }

    #[test]
    fn the_same_object_inside_prose_is_left_alone() {
        // The model explaining a tool call is not making one.
        let mut c = from_content(
            "You would call it like {\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}} \
             to list the directory.",
        );
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_none());
        assert!(c.tool_calls.is_empty());
    }

    #[test]
    fn a_name_that_is_not_an_advertised_tool_is_left_alone() {
        // A promoted call to a tool that does not exist is a hallucination
        // given hands.
        let mut c = from_content("<function=deploy>\n<parameter=env>prod</parameter>\n</function>");
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_none());
        assert!(c.tool_calls.is_empty());
        assert!(c.content.unwrap().contains("<function=deploy>"), "kept verbatim as text");
    }

    #[test]
    fn a_truncated_block_stays_text() {
        // No `</parameter>`: the reply was cut off mid-call. Half a command is
        // worse than none.
        let mut c = from_content("<function=bash>\n<parameter=command>\nrm -rf /tm");
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_none());
        assert!(c.tool_calls.is_empty());
    }

    #[test]
    fn malformed_json_stays_text() {
        let mut c = from_content("```json\n{\"name\": \"bash\", \"arguments\": {oops}}\n```");
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_none());
        assert!(c.tool_calls.is_empty());
    }

    #[test]
    fn structured_calls_present_means_the_prose_is_never_reinterpreted() {
        // The one rule that makes the whole thing safe. A model that produced a
        // real call *and* wrote about another one is already understood, and
        // inventing the second would be acting on something it did not ask for.
        let mut c = Completion {
            content: Some(
                "<function=bash>\n<parameter=command>rm -rf /</parameter>\n</function>".into(),
            ),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            }],
            usage: Usage::default(),
            ..Completion::default()
        };
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_none());
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "read");
    }

    #[test]
    fn two_blocks_in_one_reply_both_come_through() {
        let mut c = from_content(
            "<function=read>\n<parameter=path>a.txt</parameter>\n</function>\nand\n\
             <function=read>\n<parameter=path>b.txt</parameter>\n</function>",
        );
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_some());
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[0].arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(c.tool_calls[1].arguments, r#"{"path":"b.txt"}"#);
        assert_ne!(c.tool_calls[0].id, c.tool_calls[1].id, "ids are distinct");
    }

    #[test]
    fn a_multiline_value_keeps_its_inner_newlines() {
        // Only the newline the convention puts either side of the value is
        // dropped. Trimming further would silently edit file content.
        let mut c = from_content(
            "<function=bash>\n<parameter=command>\nline one\nline two\n</parameter>\n</function>",
        );
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_some());
        assert_eq!(c.tool_calls[0].arguments, r#"{"command":"line one\nline two"}"#);
    }

    #[test]
    fn ordinary_prose_and_ordinary_code_fences_are_untouched() {
        let mut c = from_content(
            "I read the file. Here is the fix:\n```rust\nfn main() {}\n```\nThat should do it.",
        );
        let before = c.content.clone();
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_none());
        assert_eq!(c.content, before, "not one byte moved");
    }

    #[test]
    fn an_empty_completion_is_still_empty() {
        let mut c = Completion::default();
        assert!(rescue_text_tool_calls(&mut c, &tools()).is_none());
    }
}
