use crate::types::{InvocationInput, RunnerError};
use jsonschema::validator_for;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) const INVOCATION_INPUT_MOUNT_PATH: &str = "/agentd/invocation-input";
const INTENT_ARTIFACT_TYPE: &str = "intent";
const INTENT_ARTIFACT_ID: &str = "operator-input";
const INTENT_SOURCE: &str = "operator";
const SUPPORTED_INTENT_CANONICAL_VERSIONS: &[&str] = &["2.0.0"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedInvocationInput {
    pub(crate) artifact_type: String,
    pub(crate) artifact_id: String,
    pub(crate) document_json: String,
}

#[derive(Debug, Default, Deserialize)]
struct MethodologyManifest {
    #[serde(default)]
    artifact_types: Vec<DeclaredArtifactType>,
}

#[derive(Debug, Deserialize)]
struct DeclaredArtifactType {
    name: String,
}

pub(crate) fn resolve_invocation_input(
    methodology_dir: &Path,
    input: Option<&InvocationInput>,
) -> Result<Option<ResolvedInvocationInput>, RunnerError> {
    let Some(input) = input else {
        return Ok(None);
    };

    let manifest = load_manifest(methodology_dir)?;
    match input {
        InvocationInput::IntentText { statement, target } => {
            let schema_path = methodology_dir.join("schemas/intent.schema.json");
            ensure_declared_artifact_type(&manifest, INTENT_ARTIFACT_TYPE).map_err(|message| {
                RunnerError::InvalidInvocationInput {
                    message: format!(
                        "methodology does not support operator intent input: {message}"
                    ),
                }
            })?;
            let schema = load_schema(&schema_path).map_err(|message| {
                RunnerError::InvalidInvocationInput {
                    message: format!(
                        "methodology does not support operator intent input: {message}"
                    ),
                }
            })?;
            ensure_supported_intent_version(&schema).map_err(|message| {
                RunnerError::InvalidInvocationInput {
                    message: format!(
                        "methodology does not support operator intent input: {message}"
                    ),
                }
            })?;

            let document = match target {
                Some(target) => json!({
                    "statement": statement,
                    "source": INTENT_SOURCE,
                    "target": target,
                }),
                None => json!({
                    "statement": statement,
                    "source": INTENT_SOURCE,
                }),
            };
            validate_document(&schema, &document, INTENT_ARTIFACT_TYPE)?;

            Ok(Some(ResolvedInvocationInput {
                artifact_type: INTENT_ARTIFACT_TYPE.to_string(),
                artifact_id: INTENT_ARTIFACT_ID.to_string(),
                document_json: render_document(&document)?,
            }))
        }
        InvocationInput::Artifact {
            artifact_type,
            artifact_id,
            document,
        } => {
            ensure_single_path_segment(artifact_type, "artifact type")?;
            ensure_single_path_segment(artifact_id, "artifact id")?;
            ensure_declared_artifact_type(&manifest, artifact_type)
                .map_err(|message| RunnerError::InvalidInvocationInput { message })?;

            let schema = load_schema(&schema_path(methodology_dir, artifact_type))
                .map_err(|message| RunnerError::InvalidInvocationInput { message })?;
            validate_document(&schema, document, artifact_type)?;

            Ok(Some(ResolvedInvocationInput {
                artifact_type: artifact_type.clone(),
                artifact_id: artifact_id.clone(),
                document_json: render_document(document)?,
            }))
        }
    }
}

fn load_manifest(methodology_dir: &Path) -> Result<MethodologyManifest, RunnerError> {
    let manifest_path = methodology_dir.join("manifest.toml");
    let manifest = fs::read_to_string(&manifest_path).map_err(RunnerError::Io)?;
    toml::from_str(&manifest).map_err(|error| RunnerError::InvalidInvocationInput {
        message: format!(
            "failed to parse methodology manifest {}: {error}",
            manifest_path.display()
        ),
    })
}

fn ensure_declared_artifact_type(
    manifest: &MethodologyManifest,
    artifact_type: &str,
) -> Result<(), String> {
    if manifest
        .artifact_types
        .iter()
        .any(|declared| declared.name == artifact_type)
    {
        return Ok(());
    }

    Err(format!(
        "artifact type '{artifact_type}' is not declared in the methodology manifest"
    ))
}

fn ensure_single_path_segment(value: &str, field_name: &str) -> Result<(), RunnerError> {
    if value.is_empty() || value.contains('/') || value.contains('\\') || value.contains('\0') {
        return Err(invalid_single_path_segment_error(field_name));
    }

    let mut components = Path::new(value).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(invalid_single_path_segment_error(field_name)),
    }
}

fn invalid_single_path_segment_error(field_name: &str) -> RunnerError {
    RunnerError::InvalidInvocationInput {
        message: format!("{field_name} must be a single path segment"),
    }
}

fn schema_path(methodology_dir: &Path, artifact_type: &str) -> PathBuf {
    methodology_dir
        .join("schemas")
        .join(format!("{artifact_type}.schema.json"))
}

fn load_schema(path: &Path) -> Result<Value, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("failed to read schema {}: {error}", path.display()))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("schema {} must contain valid JSON: {error}", path.display()))
}

fn ensure_supported_intent_version(schema: &Value) -> Result<(), String> {
    let version = schema
        .get("x-tesserine-canonical")
        .and_then(|value| value.get("version"))
        .and_then(Value::as_str)
        .ok_or_else(|| "intent schema must declare x-tesserine-canonical.version".to_string())?;
    if SUPPORTED_INTENT_CANONICAL_VERSIONS.contains(&version) {
        return Ok(());
    }

    Err(format!(
        "canonical intent version {version} is not supported"
    ))
}

fn validate_document(
    schema: &Value,
    document: &Value,
    artifact_type: &str,
) -> Result<(), RunnerError> {
    let validator = validator_for(schema).map_err(|error| RunnerError::InvalidInvocationInput {
        message: format!("schema for artifact type '{artifact_type}' is invalid: {error}"),
    })?;
    let mut errors = validator.iter_errors(document);
    if let Some(error) = errors.next() {
        return Err(RunnerError::InvalidInvocationInput {
            message: format!(
                "artifact input for type '{artifact_type}' does not match the methodology schema: {error}"
            ),
        });
    }

    Ok(())
}

fn render_document(document: &Value) -> Result<String, RunnerError> {
    serde_json::to_string(document).map_err(|error| RunnerError::InvalidInvocationInput {
        message: format!("failed to serialize invocation input document: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::resolve_invocation_input;
    use crate::types::{InvocationInput, RunnerError};
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_invocation_input_authors_v2_intent_without_absent_target() {
        let methodology_dir = unique_methodology_dir("intent-v2-without-target");
        write_intent_methodology_fixture(&methodology_dir, "2.0.0");

        let resolved = resolve_invocation_input(
            &methodology_dir,
            Some(&InvocationInput::IntentText {
                statement: "Add a status page".to_string(),
                target: None,
            }),
        )
        .expect("v2 intent input should resolve")
        .expect("intent input should materialize");

        assert_eq!(resolved.artifact_type, "intent");
        assert_eq!(resolved.artifact_id, "operator-input");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&resolved.document_json)
                .expect("resolved intent should be valid JSON"),
            json!({
                "statement": "Add a status page",
                "source": "operator",
            })
        );

        fs::remove_dir_all(&methodology_dir).expect("temporary methodology dir should be removed");
    }

    #[test]
    fn resolve_invocation_input_passes_v2_intent_target_verbatim() {
        let methodology_dir = unique_methodology_dir("intent-v2-with-target");
        write_intent_methodology_fixture(&methodology_dir, "2.0.0");

        let resolved = resolve_invocation_input(
            &methodology_dir,
            Some(&InvocationInput::IntentText {
                statement: "Work on the tracker item".to_string(),
                target: Some("tesserine/agentd#154".to_string()),
            }),
        )
        .expect("target-bearing v2 intent input should resolve")
        .expect("intent input should materialize");

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&resolved.document_json)
                .expect("resolved intent should be valid JSON"),
            json!({
                "statement": "Work on the tracker item",
                "source": "operator",
                "target": "tesserine/agentd#154",
            })
        );

        fs::remove_dir_all(&methodology_dir).expect("temporary methodology dir should be removed");
    }

    #[test]
    fn resolve_invocation_input_rejects_legacy_intent_schema_version() {
        let methodology_dir = unique_methodology_dir("intent-v1");
        write_intent_methodology_fixture(&methodology_dir, "1.0.0");

        let error = resolve_invocation_input(
            &methodology_dir,
            Some(&InvocationInput::IntentText {
                statement: "Add a status page".to_string(),
                target: None,
            }),
        )
        .expect_err("legacy canonical intent schema should be rejected");

        assert!(
            error
                .to_string()
                .contains("canonical intent version 1.0.0 is not supported"),
            "expected unsupported-version message, got {error}"
        );

        fs::remove_dir_all(&methodology_dir).expect("temporary methodology dir should be removed");
    }

    #[test]
    fn resolve_invocation_input_accepts_single_segment_artifact_names() {
        let methodology_dir = unique_methodology_dir("single-segment");
        write_methodology_fixture(&methodology_dir, "claim");

        let resolved = resolve_invocation_input(
            &methodology_dir,
            Some(&InvocationInput::Artifact {
                artifact_type: "claim".to_string(),
                artifact_id: "claim-42".to_string(),
                document: json!({ "summary": "Ship it" }),
            }),
        )
        .expect("single-segment artifact names should be accepted")
        .expect("artifact input should resolve");

        assert_eq!(resolved.artifact_type, "claim");
        assert_eq!(resolved.artifact_id, "claim-42");

        fs::remove_dir_all(&methodology_dir).expect("temporary methodology dir should be removed");
    }

    #[test]
    fn resolve_invocation_input_rejects_non_segment_artifact_names() {
        for invalid_artifact_name in [
            "",
            ".",
            "..",
            "claim/child",
            "/absolute",
            r"claim\escape",
            "claim\0escape",
        ] {
            let methodology_dir = unique_methodology_dir("invalid-segment");
            write_methodology_fixture(&methodology_dir, "claim");

            let error = resolve_invocation_input(
                &methodology_dir,
                Some(&InvocationInput::Artifact {
                    artifact_type: invalid_artifact_name.to_string(),
                    artifact_id: "claim-42".to_string(),
                    document: json!({ "summary": "Ship it" }),
                }),
            )
            .expect_err("invalid artifact type should be rejected");

            assert_invalid_invocation_input(error, invalid_artifact_name);

            let error = resolve_invocation_input(
                &methodology_dir,
                Some(&InvocationInput::Artifact {
                    artifact_type: "claim".to_string(),
                    artifact_id: invalid_artifact_name.to_string(),
                    document: json!({ "summary": "Ship it" }),
                }),
            )
            .expect_err("invalid artifact id should be rejected");

            assert_invalid_invocation_input(error, invalid_artifact_name);

            fs::remove_dir_all(&methodology_dir)
                .expect("temporary methodology dir should be removed");
        }
    }

    fn assert_invalid_invocation_input(error: RunnerError, invalid_artifact_name: &str) {
        let message = error.to_string();
        assert!(
            matches!(error, RunnerError::InvalidInvocationInput { .. }),
            "expected InvalidInvocationInput for {invalid_artifact_name:?}, got {error:?}"
        );
        assert!(
            message.contains("single path segment"),
            "expected single-path-segment guidance for {invalid_artifact_name:?}, got {message}"
        );
    }

    fn write_methodology_fixture(methodology_dir: &Path, artifact_type: &str) {
        fs::create_dir_all(methodology_dir.join("schemas"))
            .expect("methodology schemas dir should be created");
        fs::write(
            methodology_dir.join("manifest.toml"),
            format!("[[artifact_types]]\nname = \"{artifact_type}\"\n"),
        )
        .expect("methodology manifest should be written");
        fs::write(
            methodology_dir
                .join("schemas")
                .join(format!("{artifact_type}.schema.json")),
            r#"{
  "type": "object",
  "properties": {
    "summary": { "type": "string" }
  },
  "required": ["summary"],
  "additionalProperties": false
}"#,
        )
        .expect("artifact schema should be written");
    }

    fn write_intent_methodology_fixture(methodology_dir: &Path, version: &str) {
        fs::create_dir_all(methodology_dir.join("schemas"))
            .expect("methodology schemas dir should be created");
        fs::write(
            methodology_dir.join("manifest.toml"),
            "[[artifact_types]]\nname = \"intent\"\n",
        )
        .expect("methodology manifest should be written");
        fs::write(
            methodology_dir.join("schemas/intent.schema.json"),
            format!(
                r#"{{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "x-tesserine-canonical": {{
    "version": "{version}",
    "schema_url": "https://example.com/intent.schema.json",
    "prose_url": "https://example.com/INTENT.md"
  }},
  "type": "object",
  "required": ["statement", "source"],
  "additionalProperties": false,
  "properties": {{
    "statement": {{ "type": "string", "minLength": 1 }},
    "source": {{ "type": "string", "minLength": 1 }},
    "target": {{ "type": "string", "minLength": 1 }}
  }}
}}
"#
            ),
        )
        .expect("intent schema should be written");
    }

    fn unique_methodology_dir(name: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        std::env::temp_dir().join(format!(
            "agentd-runner-input-test-{name}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed),
        ))
    }
}
