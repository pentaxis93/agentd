use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root must exist")
        .to_path_buf()
}

fn read_workspace_file(path: &str) -> String {
    let full = workspace_root().join(path);
    fs::read_to_string(&full).unwrap_or_else(|e| panic!("failed reading {}: {e}", full.display()))
}

#[test]
fn architecture_doc_has_required_sections_and_is_not_placeholder() {
    let architecture = read_workspace_file("ARCHITECTURE.md");

    assert!(
        !architecture.trim().eq("See issue #3"),
        "ARCHITECTURE.md must not remain a placeholder"
    );

    for section in [
        "# Architecture",
        "## What agentd Is",
        "## Agent Capability Needs",
        "## The Plugin Boundary",
        "## Crate Layout and Constraint Mapping",
        "## What agentd Is Not",
        "## Verification Matrix",
    ] {
        assert!(
            architecture.contains(section),
            "missing architecture section: {section}"
        );
    }
}

#[test]
fn architecture_doc_explicitly_states_seven_capability_needs_as_requirements() {
    let architecture = read_workspace_file("ARCHITECTURE.md");

    for need in [
        "Network",
        "Credentials",
        "Identity",
        "Mission",
        "Tools",
        "Context",
        "Skills",
    ] {
        let marker = format!("### {need}");
        assert!(
            architecture.contains(&marker),
            "missing capability need section: {need}"
        );
    }

    let requirement_count = architecture.matches("The system must").count();
    assert!(
        requirement_count >= 7,
        "expected at least seven explicit requirement statements, found {requirement_count}"
    );
}

#[test]
fn architecture_doc_captures_plugin_boundary_and_all_workspace_crates() {
    let architecture = read_workspace_file("ARCHITECTURE.md");

    for boundary_marker in ["Framework responsibilities", "Plugin responsibilities"] {
        assert!(
            architecture.contains(boundary_marker),
            "missing plugin boundary marker: {boundary_marker}"
        );
    }

    for crate_name in [
        "agentd",
        "agentd-runner",
        "agentd-scheduler",
        "mcp-transport",
        "forgejo-mcp",
    ] {
        assert!(
            architecture.contains(crate_name),
            "missing crate mapping entry for {crate_name}"
        );
    }
}

#[test]
fn readme_and_agents_claims_match_architecture_doc() {
    let architecture = read_workspace_file("ARCHITECTURE.md");
    let readme = read_workspace_file("README.md");
    let agents = read_workspace_file("AGENTS.md");

    assert!(
        readme.contains("ARCHITECTURE.md"),
        "README.md should reference ARCHITECTURE.md"
    );
    assert!(
        agents.contains("ARCHITECTURE.md"),
        "AGENTS.md should reference ARCHITECTURE.md"
    );
    assert!(
        architecture.contains("design rationale"),
        "ARCHITECTURE.md should include design rationale language"
    );
}
