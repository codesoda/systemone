//! Jev wire format ⇄ neutral types.
//!
//! Parsing is strict about *structure* (duplicate keys, unknown fields,
//! depth, types) and permissive about *values*: floats, nulls and empty
//! entries reach the adapter untouched so each host can apply its own
//! documented rules. Rendering rounds every probability to two decimals
//! without renormalizing, matching the reference SDK fixtures.

use serde::{
    Deserialize,
    de::{self, Deserializer, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use systemone_core::{
    Answer, ChoiceQuestion, DecisionRequest, DecisionResponse, HostError, NoulQuestion, Question,
    ScoreQuestion,
};

/// Maximum nesting depth accepted on the wire.
pub const MAX_JSON_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError {
    /// `invalid_json` or `validation_error`.
    pub error_type: &'static str,
    pub message: String,
}

impl WireError {
    fn bad_json(message: impl Into<String>) -> Self {
        Self {
            error_type: "invalid_json",
            message: message.into(),
        }
    }

    fn validation(message: impl Into<String>) -> Self {
        Self {
            error_type: "validation_error",
            message: message.into(),
        }
    }
}

impl From<HostError> for WireError {
    fn from(error: HostError) -> Self {
        Self::validation(error.to_string())
    }
}

/// A parsed request plus the SystemOne routing extension.
#[derive(Debug, PartialEq)]
pub struct ParsedRequest {
    pub request: DecisionRequest,
    /// Top-level `backend` field, if present.
    pub backend: Option<String>,
}

pub fn parse_request(bytes: &[u8]) -> Result<ParsedRequest, WireError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| WireError::bad_json("request body must be valid UTF-8 JSON"))?;
    let value = parse_strict(text)?;
    let mut request = into_object(value, "request")?;
    let model = match request.shift_remove("model") {
        None => None,
        Some(Value::String(model)) if !model.is_empty() => Some(model),
        Some(_) => {
            return Err(WireError::validation(
                "model must be a nonempty string when provided",
            ));
        }
    };
    let backend = match request.shift_remove("backend") {
        None => None,
        Some(Value::String(backend)) if !backend.is_empty() => Some(backend),
        Some(_) => {
            return Err(WireError::validation(
                "backend must be a nonempty string when provided",
            ));
        }
    };
    let state = request
        .shift_remove("state")
        .ok_or_else(|| WireError::validation("state is required"))?;
    require_entry_type(&state, "state")?;
    let questions = request
        .shift_remove("questions")
        .ok_or_else(|| WireError::validation("questions is required"))?;
    reject_unknown(&request, "request")?;
    let questions = into_object(questions, "questions")?;
    if questions.is_empty() {
        return Err(WireError::validation(
            "questions must contain at least one question",
        ));
    }
    let mut parsed = Vec::with_capacity(questions.len());
    for (external_id, question) in questions {
        let question = parse_question(&external_id, question)?;
        parsed.push((external_id, question));
    }
    let request = DecisionRequest::new(model, state, parsed)?;
    Ok(ParsedRequest { request, backend })
}

fn parse_question(external_id: &str, value: Value) -> Result<Question, WireError> {
    let path = format!("questions.{external_id:?}");
    let mut question = into_object(value, &path)?;
    let kind = match question.shift_remove("type") {
        Some(Value::String(kind)) => kind,
        Some(_) => {
            return Err(WireError::validation(format!(
                "{path}.type must be a string"
            )));
        }
        None => return Err(WireError::validation(format!("{path}.type is required"))),
    };
    let instructions = question.shift_remove("instructions");
    if let Some(instructions) = &instructions {
        require_entry_type(instructions, &format!("{path}.instructions"))?;
    }
    let criteria = question.shift_remove("criteria");
    reject_unknown(&question, &path)?;
    match kind.as_str() {
        "choice" => {
            let criteria = criteria.ok_or_else(|| {
                WireError::validation(format!("{path}.criteria is required for choice"))
            })?;
            let criteria = into_object(criteria, &format!("{path}.criteria"))?;
            if criteria.is_empty() {
                return Err(WireError::validation(format!(
                    "{path}.criteria must contain at least one option"
                )));
            }
            let mut pairs = Vec::with_capacity(criteria.len());
            for (label, description) in criteria {
                require_entry_type(&description, &format!("{path}.criteria.{label:?}"))?;
                pairs.push((label, description));
            }
            Ok(Question::Choice(ChoiceQuestion {
                instructions,
                criteria: pairs,
            }))
        }
        "noul" => {
            let mut true_description = None;
            let mut false_description = None;
            if let Some(criteria) = criteria
                && !criteria.is_null()
            {
                let mut criteria = into_object(criteria, &format!("{path}.criteria"))?;
                if let Some(value) = criteria.shift_remove("true") {
                    require_entry_type(&value, &format!("{path}.criteria.true"))?;
                    true_description = Some(value);
                }
                if let Some(value) = criteria.shift_remove("false") {
                    require_entry_type(&value, &format!("{path}.criteria.false"))?;
                    false_description = Some(value);
                }
                reject_unknown(&criteria, &format!("{path}.criteria"))?;
            }
            Ok(Question::Noul(NoulQuestion {
                instructions,
                true_description,
                false_description,
            }))
        }
        "score" => {
            let levels = match criteria {
                Some(Value::Array(levels)) => levels,
                Some(_) => {
                    return Err(WireError::validation(format!(
                        "{path}.criteria must be an array for score"
                    )));
                }
                None => {
                    return Err(WireError::validation(format!(
                        "{path}.criteria is required for score"
                    )));
                }
            };
            if levels.len() < 2 {
                return Err(WireError::validation(format!(
                    "{path}.criteria must contain at least two levels"
                )));
            }
            for (index, level) in levels.iter().enumerate() {
                require_entry_type(level, &format!("{path}.criteria[{index}]"))?;
            }
            Ok(Question::Score(ScoreQuestion {
                instructions,
                levels,
            }))
        }
        _ => Err(WireError::validation(format!(
            "{path}.type must be choice, noul, or score"
        ))),
    }
}

fn require_entry_type(value: &Value, path: &str) -> Result<(), WireError> {
    if matches!(
        value,
        Value::String(_) | Value::Object(_) | Value::Array(_) | Value::Null
    ) {
        Ok(())
    } else {
        Err(WireError::validation(format!(
            "{path} must be a string, object, array, or null"
        )))
    }
}

fn into_object(value: Value, path: &str) -> Result<Map<String, Value>, WireError> {
    match value {
        Value::Object(value) => Ok(value),
        _ => Err(WireError::validation(format!(
            "{path} must be a JSON object"
        ))),
    }
}

fn reject_unknown(fields: &Map<String, Value>, path: &str) -> Result<(), WireError> {
    if let Some(field) = fields.keys().next() {
        Err(WireError::validation(format!(
            "unsupported field {field:?} in {path}"
        )))
    } else {
        Ok(())
    }
}

/// Parse JSON preserving object order, rejecting duplicate keys and nesting
/// deeper than [`MAX_JSON_DEPTH`].
pub fn parse_strict(text: &str) -> Result<Value, WireError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = deserializer
        .deserialize_any(StrictVisitor { depth: 0 })
        .map_err(classify)?;
    deserializer
        .end()
        .map_err(|error| WireError::bad_json(format!("trailing characters: {error}")))?;
    Ok(value)
}

fn classify(error: serde_json::Error) -> WireError {
    let message = error.to_string();
    if message.starts_with("duplicate JSON key") || message.starts_with("JSON nesting exceeds") {
        WireError::validation(message)
    } else {
        WireError::bad_json(message)
    }
}

struct StrictVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(de::Error::custom(format!(
                "JSON nesting exceeds maximum depth {MAX_JSON_DEPTH}"
            )));
        }
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(StrictSeed {
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(de::Error::custom(format!(
                "JSON nesting exceeds maximum depth {MAX_JSON_DEPTH}"
            )));
        }
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(StrictSeed {
                depth: self.depth + 1,
            })?;
            if object.insert(key.clone(), value).is_some() {
                return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
            }
        }
        Ok(Value::Object(object))
    }
}

struct StrictSeed {
    depth: usize,
}

impl<'de> de::DeserializeSeed<'de> for StrictSeed {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(StrictVisitor { depth: self.depth })
    }
}

/// Body shape returned by `POST /v1/systemone`.
#[derive(Debug, Deserialize, PartialEq, serde::Serialize)]
pub struct SystemOneBody {
    pub model: String,
    pub answers: Map<String, Value>,
    pub usage: Map<String, Value>,
}

/// Round `value` to two decimals for the wire without renormalizing.
#[must_use]
pub fn round_wire(value: f64) -> f64 {
    let rounded = (value * 100.0).round() / 100.0;
    if rounded == 0.0 { 0.0 } else { rounded }
}

#[must_use]
pub fn render_response(response: &DecisionResponse) -> SystemOneBody {
    let mut answers = Map::new();
    for (id, answer) in &response.answers {
        answers.insert(id.clone(), render_answer(answer));
    }
    let mut usage = Map::new();
    // Unknown usage is omitted rather than reported as zero.
    if let Some(input) = response.usage.input_tokens {
        usage.insert("input_tokens".to_owned(), Value::from(input));
    }
    if let Some(output) = response.usage.output_tokens {
        usage.insert("output_tokens".to_owned(), Value::from(output));
    }
    if let Some(cost) = response.usage.cost {
        usage.insert("cost".to_owned(), Value::from(cost));
    }
    SystemOneBody {
        model: response.model.clone(),
        answers,
        usage,
    }
}

fn render_answer(answer: &Answer) -> Value {
    let mut object = Map::new();
    match answer {
        Answer::Choice(choice) => {
            object.insert("type".to_owned(), Value::from("choice"));
            object.insert("choice".to_owned(), Value::from(choice.choice.clone()));
            if let Some(confidence) = choice.confidence {
                object.insert("confidence".to_owned(), Value::from(round_wire(confidence)));
            }
            let mut probabilities = Map::new();
            for (label, probability) in &choice.probabilities {
                probabilities.insert(label.clone(), Value::from(round_wire(*probability)));
            }
            object.insert("probabilities".to_owned(), Value::Object(probabilities));
        }
        Answer::Noul(noul) => {
            object.insert("type".to_owned(), Value::from("noul"));
            object.insert(
                "noul".to_owned(),
                Value::from(round_wire(noul.probability_true)),
            );
        }
        Answer::Score(score) => {
            object.insert("type".to_owned(), Value::from("score"));
            object.insert("score".to_owned(), Value::from(round_wire(score.score)));
            if let Some(confidence) = score.confidence {
                object.insert("confidence".to_owned(), Value::from(round_wire(confidence)));
            }
            let mut legend = Map::new();
            let mut probabilities = Map::new();
            for (index, (description, probability)) in
                score.legend.iter().zip(&score.probabilities).enumerate()
            {
                legend.insert(index.to_string(), description.clone());
                probabilities.insert(index.to_string(), Value::from(round_wire(*probability)));
            }
            object.insert("legend".to_owned(), Value::Object(legend));
            object.insert("probabilities".to_owned(), Value::Object(probabilities));
        }
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use systemone_core::{ChoiceAnswer, Diagnostics, NoulAnswer, ScoreAnswer, Usage};

    use super::*;

    #[test]
    fn parses_all_types_preserving_order_values_and_backend_selector() {
        let parsed = parse_request(br#"{
          "model":"jev-latest","backend":"cloud","state":{"z":1.5,"a":null},"questions":{
            "\u96ea":{"type":"choice","instructions":{"task":"pick"},"criteria":{"b":null,"a":{"why":"A"}}},
            "truth":{"type":"noul","criteria":{"true":{"means":"yes"},"false":"Nope"}},
            "score":{"type":"score","instructions":[],"criteria":[null,{"level":"high"}]}
          }
        }"#).unwrap();
        assert_eq!(parsed.backend.as_deref(), Some("cloud"));
        assert_eq!(parsed.request.model.as_deref(), Some("jev-latest"));
        assert_eq!(parsed.request.state["z"], 1.5);
        assert_eq!(parsed.request.questions[0].0, "雪");
        match &parsed.request.questions[0].1 {
            Question::Choice(choice) => {
                assert_eq!(choice.criteria[0].0, "b");
                assert_eq!(choice.criteria[1].1["why"], "A");
            }
            other => panic!("{other:?}"),
        }
        match &parsed.request.questions[1].1 {
            Question::Noul(noul) => assert_eq!(noul.false_description, Some("Nope".into())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rejects_structural_errors() {
        for body in [
            br#"{"state":"s","questions":{},"extra":null}"#.as_slice(),
            br#"{"state":"s","questions":{"q":{"type":"choice","criteria":{}}}}"#,
            br#"{"state":"s","questions":{"q":{"type":"score","criteria":["one"]}}}"#,
            br#"{"state":"s","questions":{"q":{"type":"noul","bogus":1}}}"#,
            br#"{"state":true,"questions":{"q":{"type":"noul"}}}"#,
            br#"{"backend":"","state":"s","questions":{"q":{"type":"noul"}}}"#,
        ] {
            assert_eq!(
                parse_request(body).unwrap_err().error_type,
                "validation_error",
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        assert_eq!(
            parse_request(br#"{"state":"s","state":"x","questions":{"q":{"type":"noul"}}}"#)
                .unwrap_err()
                .error_type,
            "validation_error"
        );
        assert_eq!(parse_request(br"{").unwrap_err().error_type, "invalid_json");
        assert_eq!(
            parse_request(br#"{"state":"s","questions":{"q":{"type":"noul"}}} x"#)
                .unwrap_err()
                .error_type,
            "invalid_json"
        );
        let deep = format!(
            "{{\"state\":{}0{},\"questions\":{{\"q\":{{\"type\":\"noul\"}}}}}}",
            "[".repeat(70),
            "]".repeat(70)
        );
        assert_eq!(
            parse_request(deep.as_bytes()).unwrap_err().error_type,
            "validation_error"
        );
    }

    #[test]
    fn renders_rounded_wire_values_and_omits_unknown_usage() {
        let response = DecisionResponse {
            model: "m".into(),
            answers: vec![
                (
                    "c".into(),
                    Answer::Choice(ChoiceAnswer {
                        choice: "first".into(),
                        probabilities: vec![
                            ("first".into(), 1.0 / 3.0),
                            ("second".into(), 2.0 / 3.0),
                        ],
                        confidence: Some(0.333),
                    }),
                ),
                (
                    "n".into(),
                    Answer::Noul(NoulAnswer {
                        probability_true: 0.126,
                    }),
                ),
                (
                    "s".into(),
                    Answer::Score(ScoreAnswer {
                        score: 1.415,
                        probabilities: vec![0.126, 0.333, 0.541],
                        confidence: None,
                        legend: vec!["low".into(), "mid".into(), "high".into()],
                    }),
                ),
            ],
            usage: Usage {
                input_tokens: Some(30),
                output_tokens: None,
                cost: None,
            },
            diagnostics: Diagnostics::default(),
        };
        let body = serde_json::to_value(render_response(&response)).unwrap();
        assert_eq!(body["answers"]["c"]["probabilities"]["first"], 0.33);
        assert_eq!(body["answers"]["c"]["probabilities"]["second"], 0.67);
        assert_eq!(body["answers"]["c"]["confidence"], 0.33);
        assert_eq!(body["answers"]["n"]["noul"], 0.13);
        assert_eq!(body["answers"]["s"]["score"], 1.42);
        assert_eq!(body["answers"]["s"]["legend"]["1"], "mid");
        assert!(body["answers"]["s"].get("confidence").is_none());
        assert_eq!(body["usage"]["input_tokens"], 30);
        assert!(body["usage"].get("output_tokens").is_none());
        let keys: Vec<_> = body["answers"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["c", "n", "s"]);
    }
}
