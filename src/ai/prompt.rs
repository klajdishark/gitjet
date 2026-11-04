//! The FunctionGemma prompt, byte for byte as the chat template renders it in training.
//!
//! Only the subset of the template that gitjet's tools use: string, boolean and string-array
//! arguments, enums, required lists. `testdata/golden_prompts.json` holds prompts rendered by
//! the real template (Python, jinja2); the tests below must match them exactly.

use serde_json::Value;

pub const SYSTEM: &str =
    "You are a model that can do function calling with the following functions";
const ESC: &str = "<escape>";

/// The part before the user's request: system line and every tool declaration. The same for
/// every request, so the model's work on it can be reused.
pub fn prefix(tools: &Value) -> String {
    let mut s = format!("<start_of_turn>developer\n{SYSTEM}");
    for tool in tools.as_array().into_iter().flatten() {
        s.push_str("<start_function_declaration>");
        s.push_str(&declaration(&tool["function"]));
        s.push_str("<end_function_declaration>");
    }
    s.push_str("<end_of_turn>\n");
    s
}

/// The user's turn and the opening of the model's turn.
pub fn turn(request: &str) -> String {
    format!(
        "<start_of_turn>user\n{}<end_of_turn>\n<start_of_turn>model\n",
        request.trim()
    )
}

fn esc(s: &str) -> String {
    format!("{ESC}{s}{ESC}")
}

fn str_list(v: &Value) -> String {
    let items: Vec<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| esc(x.as_str().unwrap_or_default()))
        .collect();
    format!("[{}]", items.join(","))
}

fn upper_type(v: &Value) -> String {
    v["type"].as_str().unwrap_or_default().to_uppercase()
}

fn declaration(f: &Value) -> String {
    let mut s = format!(
        "declaration:{}{{description:{}",
        f["name"].as_str().unwrap_or_default(),
        esc(f["description"].as_str().unwrap_or_default())
    );
    let params = &f["parameters"];
    if params.is_object() {
        s.push_str(",parameters:{");
        if let Some(props) = params["properties"].as_object().filter(|p| !p.is_empty()) {
            s.push_str("properties:{");
            s.push_str(&properties(props));
            s.push_str("},");
        }
        if params["required"].as_array().is_some_and(|r| !r.is_empty()) {
            s.push_str("required:");
            s.push_str(&str_list(&params["required"]));
            s.push(',');
        }
        if params["type"].is_string() {
            s.push_str(&format!("type:{}}}", esc(&upper_type(params))));
        }
    }
    s.push('}');
    s
}

fn properties(props: &serde_json::Map<String, Value>) -> String {
    // serde_json's Map is a BTreeMap here (no preserve_order), so keys come sorted, as
    // the template's `dictsort` does.
    let mut out = Vec::new();
    for (key, value) in props {
        if ["description", "type", "properties", "required", "nullable"].contains(&key.as_str()) {
            continue;
        }
        let mut s = format!(
            "{key}:{{description:{}",
            esc(value["description"].as_str().unwrap_or_default())
        );
        match upper_type(value).as_str() {
            "STRING" if value["enum"].as_array().is_some_and(|e| !e.is_empty()) => {
                s.push_str(&format!(",enum:{}", str_list(&value["enum"])));
            }
            "ARRAY" if value["items"].as_object().is_some_and(|i| !i.is_empty()) => {
                // gitjet's arrays are lists of strings: items is just {type: string}.
                s.push_str(&format!(
                    ",items:{{type:{}}}",
                    esc(&upper_type(&value["items"]))
                ));
            }
            _ => {}
        }
        s.push_str(&format!(",type:{}}}", esc(&upper_type(value))));
        out.push(s);
    }
    out.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("testdata/golden_prompts.json")).unwrap()
    }

    #[test]
    fn prefix_matches_the_training_template() {
        let want = fixture()["prefix"].as_str().unwrap().to_string();
        let got = prefix(&crate::ai::tools::dump());
        if got != want {
            let i = got
                .bytes()
                .zip(want.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            panic!(
                "prompt differs at byte {i}:\n  rust:     {:?}\n  template: {:?}",
                &got[i..(i + 80).min(got.len())],
                &want[i..(i + 80).min(want.len())]
            );
        }
    }

    #[test]
    fn every_golden_turn_matches_the_training_template() {
        for case in fixture()["cases"].as_array().unwrap() {
            assert_eq!(
                turn(case["request"].as_str().unwrap()),
                case["turn"].as_str().unwrap()
            );
        }
    }
}
