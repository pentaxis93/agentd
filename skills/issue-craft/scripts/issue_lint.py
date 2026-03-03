#!/usr/bin/env python3
"""Deterministic lint checks for issue bodies used by issue-craft."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

HEADING_ALIASES = {
    "acceptance criteria": "acceptance criteria",
    "acceptance criteria:": "acceptance criteria",
    "expected behavior": "expected behaviour",
    "expected behavior:": "expected behaviour",
    "expected behaviour": "expected behaviour",
    "expected behaviour:": "expected behaviour",
}

SCHEMAS = {
    "task": {
        "required": ["summary", "scope", "acceptance criteria"],
        "checklist_section": "acceptance criteria",
    },
    "epic": {
        "required": ["summary", "task issues", "acceptance criteria"],
        "checklist_section": "acceptance criteria",
    },
    "bug": {
        "required": ["bug", "reproduction", "expected behaviour", "acceptance criteria"],
        "checklist_section": "acceptance criteria",
    },
    "spike": {
        "required": ["question", "time box", "expected output"],
        "checklist_section": "expected output",
    },
}



def load_text(path: str | None) -> str:
    if path is None or path == "-":
        return sys.stdin.read()
    return Path(path).read_text(encoding="utf-8")



def normalize_heading(heading: str) -> str:
    collapsed = re.sub(r"\s+", " ", heading.strip().lower())
    return HEADING_ALIASES.get(collapsed, collapsed)



def extract_sections(text: str) -> dict[str, str]:
    headings = list(re.finditer(r"^##\s+(.+?)\s*$", text, re.MULTILINE))
    sections: dict[str, str] = {}

    for index, match in enumerate(headings):
        normalized_heading = normalize_heading(match.group(1))
        start = match.end()
        if index + 1 < len(headings):
            end = headings[index + 1].start()
        else:
            end = len(text)

        # Keep first section for each heading label for deterministic behavior.
        sections.setdefault(normalized_heading, text[start:end])

    return sections



def run_checks(text: str, issue_type: str | None = None) -> dict:
    errors: list[str] = []
    warnings: list[str] = []
    sections = extract_sections(text)

    if issue_type:
        selected_type = issue_type
        schema = SCHEMAS[selected_type]
        missing = [heading for heading in schema["required"] if heading not in sections]
        if missing:
            errors.append(
                f"{selected_type} issue missing headings: " + ", ".join(missing)
            )
    else:
        candidates: list[tuple[str, list[str]]] = []
        for candidate_type, schema in SCHEMAS.items():
            missing = [heading for heading in schema["required"] if heading not in sections]
            candidates.append((candidate_type, missing))

        matches = [candidate_type for candidate_type, missing in candidates if not missing]
        if not matches:
            errors.append("issue does not match known template headings")
            for candidate_type, missing in candidates:
                errors.append(f"{candidate_type} missing headings: {', '.join(missing)}")
            return {
                "passed": False,
                "issue_type": None,
                "errors": errors,
                "warnings": warnings,
            }

        selected_type = matches[0]
        schema = SCHEMAS[selected_type]

    checklist_section = schema["checklist_section"]
    checklist_block = sections.get(checklist_section, "")
    if checklist_block:
        checklist_items = re.findall(r"^-\s+\[\s?[xX ]\]\s+.+$", checklist_block, re.MULTILINE)
        if not checklist_items:
            errors.append(f"{checklist_section} must include markdown checkboxes")

    scope_block = sections.get("scope", "")
    if scope_block and re.search(r"\b(various|across the codebase|misc)\b", scope_block, re.IGNORECASE):
        warnings.append("scope contains broad wording; consider naming exact files/modules")

    size_block = sections.get("size", "")
    if size_block and not re.search(r"\bsmall\b|\bmedium\b|\blarge\b", size_block, re.IGNORECASE):
        warnings.append("size section should declare small/medium/large")

    if re.search(r"\b(depends on|parent)\b", text, re.IGNORECASE) and not re.search(r"#\d+", text):
        warnings.append("dependencies mentioned without issue references")

    passed = not errors
    return {
        "passed": passed,
        "issue_type": selected_type,
        "errors": errors,
        "warnings": warnings,
    }



def main() -> int:
    parser = argparse.ArgumentParser(
        description="Lint issue markdown for deterministic structural checks."
    )
    parser.add_argument("path", nargs="?", help="Issue markdown path or '-' for stdin")
    parser.add_argument(
        "--type",
        choices=sorted(SCHEMAS.keys()),
        help="Issue type for strict schema validation",
    )
    parser.add_argument(
        "--json", action="store_true", help="Output machine-readable JSON report"
    )
    args = parser.parse_args()

    text = load_text(args.path)
    report = run_checks(text, issue_type=args.type)

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print("PASS" if report["passed"] else "FAIL")
        if report["issue_type"]:
            print(f"Type: {report['issue_type']}")
        for error in report["errors"]:
            print(f"ERROR: {error}")
        for warning in report["warnings"]:
            print(f"WARN: {warning}")

    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
