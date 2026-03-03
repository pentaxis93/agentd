# Language Patterns

Concrete BDD patterns for common languages. Each section shows how to
apply sentence-named behaviours and Given/When/Then structure using the
language's native test tooling.

## Table of Contents

- [Naming Conventions](#naming-conventions)
- [Rust](#rust)
- [Python](#python)
- [TypeScript / JavaScript](#typescript--javascript)
- [When to Use a BDD Framework](#when-to-use-a-bdd-framework)

---

## Naming Conventions

The naming pattern adapts to each language's conventions while
preserving the core rule: test names are sentences describing behaviour.

| Language | Convention | Example |
|----------|-----------|---------|
| Rust | `should_` prefix, snake_case | `should_reject_empty_name` |
| Python | `should_` prefix, snake_case | `test_should_reject_empty_name` |
| TypeScript | `it("should ...")` or `test("should ...")` | `it("should reject empty name")` |
| Go | `TestShould` prefix, PascalCase | `TestShouldRejectEmptyName` |
| Java | `should` prefix, camelCase | `shouldRejectEmptyName` |

The `should` prefix enforces the sentence template from Dan North:
"The module **should** do something." If a name doesn't fit this
template, the behaviour may belong elsewhere.

---

## Rust

### Module structure

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_expand_tilde_to_home_directory() {
        // Given
        let home = "/home/user";
        let input = "~/config/loadout.toml";

        // When
        let result = expand_path(input, home);

        // Then
        assert_eq!(result, PathBuf::from("/home/user/config/loadout.toml"));
    }

    #[test]
    fn should_return_path_unchanged_when_no_tilde() {
        // Given
        let input = "/absolute/path/file.toml";

        // When
        let result = expand_path(input, "/home/user");

        // Then
        assert_eq!(result, PathBuf::from("/absolute/path/file.toml"));
    }
}
```

Reading the test names as a specification:

```
expand_path
  - should expand tilde to home directory
  - should return path unchanged when no tilde
```

### Integration tests with temp directories

```rust
use tempfile::TempDir;

#[test]
fn should_create_symlink_to_skill_source() {
    // Given
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("source/my-skill");
    let target = tmp.path().join("target");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("SKILL.md"), "---\nname: my-skill\n---").unwrap();
    fs::create_dir_all(&target).unwrap();

    // When
    link_skill(&source, &target).unwrap();

    // Then
    let link = target.join("my-skill");
    assert!(link.is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), source);
}
```

### Error behaviours

```rust
#[test]
fn should_return_error_when_config_file_missing() {
    // Given
    let path = PathBuf::from("/nonexistent/loadout.toml");

    // When
    let result = load_config(&path);

    // Then
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}
```

### Naming patterns for common scenarios

| Scenario | Test name |
|----------|-----------|
| Happy path | `should_parse_valid_config` |
| Missing input | `should_return_error_when_config_missing` |
| Invalid input | `should_reject_name_with_uppercase` |
| Edge case | `should_handle_empty_sources_list` |
| Boundary | `should_reject_name_longer_than_64_chars` |
| State-dependent | `should_skip_existing_symlink_when_unchanged` |

---

## Python

### pytest with BDD naming

```python
class TestConfigLoader:

    def test_should_parse_valid_toml_config(self, tmp_path):
        # Given
        config_file = tmp_path / "loadout.toml"
        config_file.write_text('[sources]\nskills = ["~/skills"]')

        # When
        config = load_config(config_file)

        # Then
        assert config.sources.skills == ["~/skills"]

    def test_should_raise_error_when_config_missing(self):
        # Given
        path = Path("/nonexistent/config.toml")

        # When / Then
        with pytest.raises(FileNotFoundError):
            load_config(path)
```

### unittest style

```python
class TestConfigLoaderBehaviour(unittest.TestCase):

    def test_should_expand_environment_variables(self):
        # Given
        os.environ["MY_DIR"] = "/custom/path"
        raw = "$MY_DIR/skills"

        # When
        result = expand_path(raw)

        # Then
        self.assertEqual(result, Path("/custom/path/skills"))
```

---

## TypeScript / JavaScript

### Jest / Vitest

```typescript
describe("ConfigLoader", () => {
  it("should parse a valid TOML config", () => {
    // Given
    const toml = `[sources]\nskills = ["~/skills"]`;

    // When
    const config = parseConfig(toml);

    // Then
    expect(config.sources.skills).toEqual(["~/skills"]);
  });

  it("should throw when config file is missing", async () => {
    // Given
    const path = "/nonexistent/config.toml";

    // When / Then
    await expect(loadConfig(path)).rejects.toThrow("not found");
  });
});
```

The `describe`/`it` pattern naturally produces readable specs:

```
ConfigLoader
  - should parse a valid TOML config
  - should throw when config file is missing
```

### Node test runner

```typescript
import { describe, it } from "node:test";
import assert from "node:assert";

describe("PathExpander", () => {
  it("should expand tilde to home directory", () => {
    // Given
    const input = "~/config/app.toml";

    // When
    const result = expandPath(input);

    // Then
    assert.strictEqual(result, `${process.env.HOME}/config/app.toml`);
  });
});
```

---

## When to Use a BDD Framework

Native test tooling with Given/When/Then comments covers most
codebases. Consider a framework when:

| Signal | Framework option |
|--------|-----------------|
| Complex domain with many scenario permutations | `cucumber` (Rust), `pytest-bdd` (Python), `cucumber-js` (TS) |
| Parameterized scenarios (same structure, varying data) | `rstest` (Rust), `pytest.mark.parametrize` (Python) |
| Stakeholders need to read/write feature files | Full Gherkin: `.feature` files + step definitions |

For most developer-facing libraries and CLIs, the lightweight
approach (sentence names + Given/When/Then comments) is sufficient.
The value of BDD is in the thinking pattern, not the framework.

### Rust BDD crates

| Crate | Style | Use when |
|-------|-------|----------|
| `rstest` | Parameterized fixtures | Many scenarios with shared setup, data-driven tests |
| `rstest-bdd` | Given/When/Then macros on rstest | Want structured BDD syntax without Gherkin files |
| `cucumber` | Full Gherkin with `.feature` files | Complex domain, stakeholder-readable specs |
