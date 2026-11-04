//! Read the model's answer: `<start_function_call>call:NAME{key:<escape>v<escape>,...}<end_function_call>`.

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::tools::{self, ToolCall};

pub const START: &str = "<start_function_call>";
pub const END: &str = "<end_function_call>";
const ESC: &str = "<escape>";

/// The tool name and JSON arguments of the first call in `text`.
pub fn parse_raw(text: &str) -> Result<(String, Value)> {
    let start = text
        .find(START)
        .ok_or_else(|| anyhow!("no function call in {text:?}"))?;
    let body = &text[start + START.len()..];
    let end = body
        .find(END)
        .ok_or_else(|| anyhow!("unfinished function call in {text:?}"))?;
    let body = body[..end]
        .strip_prefix("call:")
        .ok_or_else(|| anyhow!("expected `call:` in {text:?}"))?;
    let open = body
        .find('{')
        .ok_or_else(|| anyhow!("no arguments in {text:?}"))?;
    let name = &body[..open];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("bad tool name {name:?}");
    }
    let inner = body[open + 1..]
        .strip_suffix('}')
        .ok_or_else(|| anyhow!("arguments not closed in {text:?}"))?;
    Ok((name.to_string(), Value::Object(args(inner)?)))
}

/// The model's answer as a typed call, checked against the registry.
pub fn parse(text: &str) -> Result<ToolCall> {
    let (name, arguments) = parse_raw(text)?;
    tools::parse_call(&name, arguments)
}

fn args(body: &str) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    let mut rest = body;
    while !rest.is_empty() {
        let colon = rest
            .find(':')
            .ok_or_else(|| anyhow!("bad argument at {rest:?}"))?;
        let key = &rest[..colon];
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("bad argument name {key:?}");
        }
        let (value, after) = value(&rest[colon + 1..])?;
        out.insert(key.to_string(), value);
        rest = after.strip_prefix(',').unwrap_or(after);
    }
    Ok(out)
}

fn value(s: &str) -> Result<(Value, &str)> {
    if let Some(r) = s.strip_prefix(ESC) {
        let end = r
            .find(ESC)
            .ok_or_else(|| anyhow!("unclosed string in {s:?}"))?;
        return Ok((Value::String(r[..end].to_string()), &r[end + ESC.len()..]));
    }
    if let Some(mut r) = s.strip_prefix('[') {
        let mut items = Vec::new();
        while !r.starts_with(']') {
            if r.is_empty() {
                bail!("unclosed list in {s:?}");
            }
            let (v, after) = value(r)?;
            items.push(v);
            r = after.strip_prefix(',').unwrap_or(after);
        }
        return Ok((Value::Array(items), &r[1..]));
    }
    // A bare word: true, false or a number.
    let end = s.find([',', '}', ']']).unwrap_or(s.len());
    let word = &s[..end];
    let v: Value = serde_json::from_str(word).map_err(|_| anyhow!("bad value {word:?}"))?;
    Ok((v, &s[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strings_lists_and_booleans() {
        let (name, a) = parse_raw(
            "<start_function_call>call:cherry_pick{commits:[<escape>3f9a2c1<escape>,<escape>8be4<escape>],onto:<escape>deploy/uat<escape>,push:true}<end_function_call><start_function_response>",
        )
        .unwrap();
        assert_eq!(name, "cherry_pick");
        assert_eq!(
            a,
            serde_json::json!({"commits": ["3f9a2c1", "8be4"], "onto": "deploy/uat", "push": true})
        );
    }

    #[test]
    fn keeps_commas_and_colons_inside_strings() {
        let (_, a) = parse_raw(
            "<start_function_call>call:clarify{question:<escape>Merge which branch, and into: which?<escape>}<end_function_call>",
        )
        .unwrap();
        assert_eq!(a["question"], "Merge which branch, and into: which?");
    }

    #[test]
    fn rejects_broken_output() {
        for bad in [
            "merge develop",
            "<start_function_call>call:merge{source:<escape>develop}<end_function_call>",
            "<start_function_call>call:merge{source:<escape>develop<escape>}",
            "<start_function_call>call:rm -rf{}<end_function_call>",
        ] {
            assert!(parse_raw(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_golden_answer_parses_into_a_typed_call() {
        let fixture: Value =
            serde_json::from_str(include_str!("testdata/golden_prompts.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let text = case["completion"].as_str().unwrap();
            parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        }
    }
}
