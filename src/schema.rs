//! Honesty checks for the JSON Schemas a server advertises for its tools.
//!
//! A client can only learn what a tool accepts from the schema returned by
//! `tools/list`. A property declared as a bare `{"type": "object"}` says
//! nothing, so a caller discovers the real keys one rejected call at a time:
//! a full request/response round trip per missing field, with no way to tell
//! an incomplete schema from wrong arguments.
//!
//! Genuinely open-ended passthrough arguments are legitimate - their keys are
//! defined by another system and cannot be enumerated - but a deliberate
//! decision must not look identical to silence.
//!
//! The rules applied by [`validate_tool_schema`], recursively:
//!
//! - a node declaring `"type": "object"` must describe what it accepts, by
//!   carrying `properties`, by giving `additionalProperties` a schema, or by
//!   declaring itself free-form with a `description` and
//!   `additionalProperties: true`;
//! - a node declaring `"type": "array"` must carry `items`.
//!
//! A node without an explicit `type` is not judged: composition keywords and
//! references describe their shape elsewhere.

use crate::{Map, Value};
use std::fmt;
use std::io;

/// Obligation an advertised schema node failed to meet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SchemaDefectKind {
    /// `"type": "object"` that describes neither named properties nor
    /// additional members, and is not declared free-form.
    UndescribedObject,
    /// `"type": "array"` without `items`.
    UndescribedArray,
}

impl fmt::Display for SchemaDefectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UndescribedObject => concat!(
                r#"declares "type": "object" without describing what it accepts; add "#,
                r#""properties", give "additionalProperties" a schema, or declare it "#,
                r#"free-form with a "description" and "additionalProperties": true"#,
            ),
            Self::UndescribedArray => r#"declares "type": "array" without "items""#,
        })
    }
}

/// One advertised schema node that does not describe what the tool accepts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaDefect {
    /// Tool the schema was registered under, when the defect came from a
    /// builder registration rather than a standalone schema check.
    pub tool: Option<String>,
    /// JSON Pointer to the offending node, rooted at the tool input schema.
    ///
    /// The root schema itself has an empty pointer, per RFC 6901.
    pub path: String,
    /// Obligation the node failed to meet.
    pub kind: SchemaDefectKind,
}

impl fmt::Display for SchemaDefect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(tool) = &self.tool {
            write!(formatter, "tool \"{tool}\": ")?;
        }
        if self.path.is_empty() {
            formatter.write_str("the input schema root ")?;
        } else {
            write!(formatter, "{} ", self.path)?;
        }
        write!(formatter, "{}", self.kind)
    }
}

impl std::error::Error for SchemaDefect {}

/// Checks that one advertised tool input schema describes what it accepts.
///
/// Servers that build their catalog by hand can call this from their own
/// tests. Both builders apply the same check at registration; see
/// `McpServer::strict_schemas`.
///
/// # Errors
///
/// Returns every defect found, in a deterministic pre-order walk of the
/// schema, so one test run reports the complete list instead of the first
/// problem.
pub fn validate_tool_schema(schema: &Value) -> Result<(), Vec<SchemaDefect>> {
    let defects = defects_in(None, schema);
    if defects.is_empty() {
        Ok(())
    } else {
        Err(defects)
    }
}

/// Collects the defects of one schema, attributed to a registered tool name.
pub(crate) fn defects_in(tool: Option<&str>, schema: &Value) -> Vec<SchemaDefect> {
    let mut defects = Vec::new();
    inspect(tool, schema, &mut String::new(), &mut defects);
    defects
}

/// Refuses to start a runtime whose catalog was registered with defects.
///
/// Reporting at startup keeps registration infallible for the existing
/// builder API while still failing before any client can be misled.
pub(crate) fn reject_defects(defects: &[SchemaDefect]) -> io::Result<()> {
    if defects.is_empty() {
        return Ok(());
    }
    let mut message = String::from("strict schemas rejected the tool catalog:");
    for defect in defects {
        message.push_str("\n  ");
        message.push_str(&defect.to_string());
    }
    Err(io::Error::new(io::ErrorKind::InvalidInput, message))
}

fn inspect(tool: Option<&str>, node: &Value, path: &mut String, defects: &mut Vec<SchemaDefect>) {
    let Some(node) = node.as_object() else {
        return;
    };
    if declares_type(node, "object") && !describes_members(node) {
        defects.push(defect(tool, path, SchemaDefectKind::UndescribedObject));
    }
    if declares_type(node, "array") && !node.contains_key("items") {
        defects.push(defect(tool, path, SchemaDefectKind::UndescribedArray));
    }

    if let Some(properties) = node.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            descend(tool, property, path, &["properties", name], defects);
        }
    }
    match node.get("items") {
        // The tuple form lists one schema per position; each carries the same
        // obligation as a named property.
        Some(Value::Array(items)) => {
            for (position, item) in items.iter().enumerate() {
                descend(tool, item, path, &["items", &position.to_string()], defects);
            }
        }
        Some(items) => descend(tool, items, path, &["items"], defects),
        None => {}
    }
    if let Some(additional) = node.get("additionalProperties") {
        descend(tool, additional, path, &["additionalProperties"], defects);
    }
}

fn descend(
    tool: Option<&str>,
    node: &Value,
    path: &mut String,
    segments: &[&str],
    defects: &mut Vec<SchemaDefect>,
) {
    let parent = path.len();
    for segment in segments {
        push_segment(path, segment);
    }
    inspect(tool, node, path, defects);
    path.truncate(parent);
}

fn describes_members(node: &Map<String, Value>) -> bool {
    if node.contains_key("properties") {
        return true;
    }
    match node.get("additionalProperties") {
        // A schema-valued `additionalProperties` describes every member even
        // though the key names are open.
        Some(Value::Object(_)) => true,
        // A free-form passthrough is legitimate only when it is stated: the
        // description is where a caller learns who defines the keys.
        Some(Value::Bool(true)) => node
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|description| !description.trim().is_empty()),
        _ => false,
    }
}

fn declares_type(node: &Map<String, Value>, name: &str) -> bool {
    match node.get("type") {
        Some(Value::String(declared)) => declared == name,
        Some(Value::Array(declared)) => declared.iter().any(|entry| entry.as_str() == Some(name)),
        _ => false,
    }
}

fn defect(tool: Option<&str>, path: &str, kind: SchemaDefectKind) -> SchemaDefect {
    SchemaDefect {
        tool: tool.map(str::to_owned),
        path: path.to_owned(),
        kind,
    }
}

/// Appends one RFC 6901 reference token, escaping `~` and `/`.
fn push_segment(path: &mut String, segment: &str) {
    path.push('/');
    for character in segment.chars() {
        match character {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            _ => path.push(character),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{reject_defects, validate_tool_schema, SchemaDefectKind};
    use crate::json;

    fn defect_paths(schema: &crate::Value) -> Vec<(String, SchemaDefectKind)> {
        validate_tool_schema(schema)
            .expect_err("schema must be rejected")
            .into_iter()
            .map(|defect| (defect.path, defect.kind))
            .collect()
    }

    #[test]
    fn a_bare_object_property_is_rejected() {
        let schema = json!({
            "type": "object",
            "properties": {
                "budget": {"type": "object"}
            },
            "required": ["budget"]
        });
        assert_eq!(
            defect_paths(&schema),
            vec![(
                "/properties/budget".to_owned(),
                SchemaDefectKind::UndescribedObject
            )]
        );
    }

    #[test]
    fn a_bare_array_property_is_rejected() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": {"type": "array"}
            }
        });
        assert_eq!(
            defect_paths(&schema),
            vec![(
                "/properties/tags".to_owned(),
                SchemaDefectKind::UndescribedArray
            )]
        );
    }

    #[test]
    fn an_explicitly_declared_free_form_object_is_accepted() {
        let schema = json!({
            "type": "object",
            "properties": {
                "passthrough": {
                    "type": "object",
                    "description": "Opaque options forwarded to the upstream planner.",
                    "additionalProperties": true
                },
                "labels": {
                    "type": "object",
                    "additionalProperties": {"type": "string"}
                }
            }
        });
        assert_eq!(validate_tool_schema(&schema), Ok(()));
    }

    #[test]
    fn a_free_form_object_without_a_description_is_rejected() {
        let schema = json!({
            "type": "object",
            "properties": {
                "passthrough": {"type": "object", "additionalProperties": true}
            }
        });
        assert_eq!(
            defect_paths(&schema),
            vec![(
                "/properties/passthrough".to_owned(),
                SchemaDefectKind::UndescribedObject
            )]
        );
    }

    #[test]
    fn a_nested_object_inside_properties_is_rejected() {
        let schema = json!({
            "type": "object",
            "properties": {
                "availability": {
                    "type": "object",
                    "properties": {
                        "window": {"type": "object"},
                        "slots": {"type": "array", "items": {"type": "object"}}
                    }
                }
            }
        });
        assert_eq!(
            defect_paths(&schema),
            vec![
                (
                    "/properties/availability/properties/slots/items".to_owned(),
                    SchemaDefectKind::UndescribedObject
                ),
                (
                    "/properties/availability/properties/window".to_owned(),
                    SchemaDefectKind::UndescribedObject
                ),
            ]
        );
    }

    #[test]
    fn a_fully_described_schema_is_accepted() {
        let schema = json!({
            "type": "object",
            "properties": {
                "budget": {
                    "type": "object",
                    "properties": {
                        "ceiling_cents": {"type": "integer"},
                        "weavatrix": {"type": "string"}
                    },
                    "required": ["ceiling_cents", "weavatrix"]
                },
                "windows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"start": {"type": "string"}}
                    }
                },
                "note": {"type": "string"}
            },
            "required": ["budget"]
        });
        assert_eq!(validate_tool_schema(&schema), Ok(()));
    }

    #[test]
    fn a_no_argument_tool_declares_an_empty_property_set() {
        assert_eq!(
            validate_tool_schema(&json!({"type": "object", "properties": {}})),
            Ok(())
        );
        assert_eq!(
            defect_paths(&json!({"type": "object"})),
            vec![(String::new(), SchemaDefectKind::UndescribedObject)]
        );
    }

    #[test]
    fn nullable_unions_and_untyped_nodes_follow_the_same_rule() {
        assert_eq!(
            defect_paths(&json!({
                "type": "object",
                "properties": {
                    "maybe": {"type": ["object", "null"]},
                    "either": {"oneOf": [{"type": "string"}]}
                }
            })),
            vec![(
                "/properties/maybe".to_owned(),
                SchemaDefectKind::UndescribedObject
            )]
        );
    }

    #[test]
    fn pointers_escape_reserved_characters_in_property_names() {
        assert_eq!(
            defect_paths(&json!({
                "type": "object",
                "properties": {"a/b~c": {"type": "array"}}
            })),
            vec![(
                "/properties/a~1b~0c".to_owned(),
                SchemaDefectKind::UndescribedArray
            )]
        );
    }

    #[test]
    fn rejection_reports_every_defect_in_one_message() {
        let defects = validate_tool_schema(&json!({
            "type": "object",
            "properties": {
                "budget": {"type": "object"},
                "tags": {"type": "array"}
            }
        }))
        .expect_err("schema must be rejected");
        let error = reject_defects(&defects).expect_err("defects must be refused");
        let message = error.to_string();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(message.contains("/properties/budget"), "{message}");
        assert!(message.contains("/properties/tags"), "{message}");
        assert!(reject_defects(&[]).is_ok());
    }
}
