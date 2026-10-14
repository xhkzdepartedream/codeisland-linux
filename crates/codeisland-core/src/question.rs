//! Reading an `AskUserQuestion` payload, and building the answer the CLI expects.
//!
//! The contract was measured on 2026-10-14 against a real interactive session --
//! see `docs/协议.md`, section 提问. What the journal actually contained:
//!
//! ```json
//! {"questions":[{"header":"Color","question":"Red or blue?","multiSelect":false,
//!   "options":[{"label":"Red","description":"The red option."},
//!              {"label":"Blue","description":"The blue option."}]}]}
//! ```
//!
//! and after an answer, `PostToolUse` showed the same `questions` plus
//! `{"answers":{"Red or blue?":"Red"},"annotations":{}}`.
//!
//! Deliberately *not* handled, because the contract is unverified for them:
//! `multiSelect` (is the value an array or a string?), and free-text or numeric
//! questions (the measured payload carries no `type` field at all). A question we
//! cannot answer correctly must produce **no card and no reply**, so the CLI asks
//! in its own terminal. Guessing an answer shape would be worse than silence.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The tool that carries questions.
pub const ASK_TOOL: &str = "AskUserQuestion";

/// One selectable option. `description` is optional upstream and rendered as a
/// subtitle, so it defaults to empty rather than failing the parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// One question the agent is asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Short eyebrow label, e.g. "Color". May be absent upstream.
    pub header: String,
    /// The question text. Named `question`, not something tidier, because that is
    /// the key the tool itself uses -- and `answers` is keyed by this exact string.
    /// Renaming it here would put a translation layer between the island and the
    /// payload the CLI validates.
    pub question: String,
    pub options: Vec<QuestionOption>,
}

/// Parse `tool_input` into answerable questions.
///
/// `None` means "this is not a question we know how to answer", which covers: a
/// different tool, a missing or empty `questions` array, a `multiSelect` question,
/// a question without options (text/number shape), or anything we cannot name a
/// label for.
pub fn parse(tool_name: Option<&str>, input: Option<&Value>) -> Option<Vec<Question>> {
    if tool_name != Some(ASK_TOOL) {
        return None;
    }
    let questions = input?.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }

    let mut out = Vec::with_capacity(questions.len());
    for q in questions {
        // Unverified shape (contract item 7): refuse rather than guess.
        if q.get("multiSelect").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let text = q.get("question")?.as_str()?;
        let options = q.get("options")?.as_array()?;
        if options.is_empty() {
            return None; // text / number question: contract item 8, unknown shape
        }
        let mut parsed = Vec::with_capacity(options.len());
        for o in options {
            let label = o.get("label")?.as_str()?;
            parsed.push(QuestionOption {
                label: label.to_string(),
                description: o
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        out.push(Question {
            header: q.get("header").and_then(Value::as_str).unwrap_or("").to_string(),
            question: text.to_string(),
            options: parsed,
        });
    }
    Some(out)
}

/// Build the `updatedInput` object: the original input untouched, plus `answers`
/// keyed by question text, plus an empty `annotations` map (as the measured
/// `PostToolUse` payload had it).
///
/// Refuses to invent anything: every question needs an answer, and every answer
/// must be one of that question's own labels. A label we did not offer is not a
/// answer we are allowed to submit.
pub fn answered_input(input: &Value, questions: &[Question], answers: &[(String, String)]) -> Option<Value> {
    let mut map = input.as_object()?.clone();
    let mut out = Map::new();

    for q in questions {
        let label = answers
            .iter()
            .find(|(text, _)| text == &q.question)
            .map(|(_, label)| label.as_str())?;
        if !q.options.iter().any(|o| o.label == label) {
            return None;
        }
        out.insert(q.question.clone(), Value::String(label.to_string()));
    }

    map.insert("answers".to_string(), Value::Object(out));
    map.entry("annotations")
        .or_insert_with(|| Value::Object(Map::new()));
    Some(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verbatim from the 2026-10-14 capture. If this stops parsing, the card is
    /// broken against the real thing, not just against a convenient fixture.
    fn measured() -> Value {
        json!({"questions": [{"header": "Color", "question": "Red or blue?", "multiSelect": false,
            "options": [{"label": "Red", "description": "The red option."},
                        {"label": "Blue", "description": "The blue option."}]}]})
    }

    #[test]
    fn parses_the_measured_payload() {
        let questions = parse(Some(ASK_TOOL), Some(&measured())).unwrap();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].header, "Color");
        assert_eq!(questions[0].question, "Red or blue?");
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(questions[0].options[1].label, "Blue");
    }

    #[test]
    fn another_tool_is_not_a_question() {
        assert!(parse(Some("Bash"), Some(&measured())).is_none());
        assert!(parse(None, Some(&measured())).is_none());
    }

    #[test]
    fn a_multi_select_question_is_refused_not_guessed() {
        let input = json!({"questions": [{"question": "Which colours?", "multiSelect": true,
            "options": [{"label": "Red"}, {"label": "Blue"}]}]});
        assert!(parse(Some(ASK_TOOL), Some(&input)).is_none(), "contract item 7 is unverified");
    }

    #[test]
    fn a_question_without_options_is_refused() {
        // The text/number shape: no `type` field was ever observed, so an option-less
        // question is not something this card can answer.
        let input = json!({"questions": [{"question": "Which version?"}]});
        assert!(parse(Some(ASK_TOOL), Some(&input)).is_none());

        let empty_options = json!({"questions": [{"question": "Which version?", "options": []}]});
        assert!(parse(Some(ASK_TOOL), Some(&empty_options)).is_none());
    }

    #[test]
    fn missing_or_empty_questions_is_refused() {
        assert!(parse(Some(ASK_TOOL), Some(&json!({}))).is_none());
        assert!(parse(Some(ASK_TOOL), Some(&json!({"questions": []}))).is_none());
        assert!(parse(Some(ASK_TOOL), None).is_none());
    }

    #[test]
    fn answers_keep_the_original_questions_intact() {
        let questions = parse(Some(ASK_TOOL), Some(&measured())).unwrap();
        let answers = vec![("Red or blue?".to_string(), "Blue".to_string())];
        let out = answered_input(&measured(), &questions, &answers).unwrap();

        assert_eq!(out["answers"]["Red or blue?"], json!("Blue"));
        assert_eq!(out["annotations"], json!({}));
        // The questions themselves must survive byte-for-byte in shape.
        assert_eq!(out["questions"], measured()["questions"]);
    }

    #[test]
    fn an_unoffered_label_is_not_an_answer() {
        let questions = parse(Some(ASK_TOOL), Some(&measured())).unwrap();
        let answers = vec![("Red or blue?".to_string(), "Green".to_string())];
        assert!(answered_input(&measured(), &questions, &answers).is_none());
    }

    #[test]
    fn every_question_needs_its_answer() {
        let questions = parse(Some(ASK_TOOL), Some(&measured())).unwrap();
        let answers = vec![("some other question".to_string(), "Blue".to_string())];
        assert!(answered_input(&measured(), &questions, &answers).is_none());
        assert!(answered_input(&measured(), &questions, &[]).is_none());
    }

    #[test]
    fn several_questions_answer_together() {
        let input = json!({"questions": [
            {"question": "Colour?", "options": [{"label": "Red"}, {"label": "Blue"}]},
            {"question": "Size?", "options": [{"label": "Small"}, {"label": "Large"}]}]});
        let questions = parse(Some(ASK_TOOL), Some(&input)).unwrap();
        assert_eq!(questions.len(), 2);
        let answers = vec![
            ("Colour?".to_string(), "Red".to_string()),
            ("Size?".to_string(), "Large".to_string()),
        ];
        let out = answered_input(&input, &questions, &answers).unwrap();
        assert_eq!(out["answers"], json!({"Colour?": "Red", "Size?": "Large"}));
    }
}
