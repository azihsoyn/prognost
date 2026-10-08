//! The JSON documents prognost prints, described as JSON Schema — derived
//! from the types themselves, so the published schema cannot drift from
//! what the commands actually emit.

/// `prognost --schema`: the shapes of `plan --json`, `assess --json` and
/// the older `--impact --json`.
pub fn schema_document() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "prognost JSON output",
        "version": SCHEMA_VERSION,
        // `prognost plan --json` prints one of these.
        "plan": schemars::schema_for!(crate::plan::PlanReport),
        // `prognost assess --json` prints one of these.
        "assess": schemars::schema_for!(crate::assess::Assessment),
        // The older `prognost --impact --json` prints one of these.
        "impact": schemars::schema_for!(crate::impact::ImpactReport),
    })
}

/// Raised when a change breaks compatibility.
pub const SCHEMA_VERSION: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA_ARTIFACT: &str = "docs/api/prognost.schema.json";

    /// Fails when the committed schema no longer matches the types, so a
    /// new request cannot be added without the published schema following
    /// it.
    #[test]
    fn generated_schema_artifact_is_current() {
        let actual = format!(
            "{}\n",
            serde_json::to_string_pretty(&schema_document()).unwrap()
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SCHEMA_ARTIFACT);
        if std::env::var_os("PROGNOST_UPDATE_API_SCHEMA").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &actual).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            expected, actual,
            "schema artifact is stale; run PROGNOST_UPDATE_API_SCHEMA=1 cargo test"
        );
    }
}
