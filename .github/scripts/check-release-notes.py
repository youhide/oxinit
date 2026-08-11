#!/usr/bin/env python3
"""Render the release notes header the way the workflow does, and check it.

The v0.1.0 notes shipped with an empty ```bash block in them. It came from
editing the heredoc by string substitution — inserting a closing fence, some
prose and an opening fence directly above a closing fence that was already
there. Nothing rendered the result before it was published.

Markdown has no compiler, so this is the compiler: unbalanced fences and empty
code blocks are the two failures that survive review by looking fine in a diff.
"""

import pathlib
import sys

WORKFLOW = pathlib.Path(".github/workflows/release.yml")
START = "cat > header.md <<'EOF'\n"
END = "\n          EOF\n"


def main() -> int:
    text = WORKFLOW.read_text()
    if START not in text or END not in text:
        print(f"{WORKFLOW}: no release-notes heredoc to check")
        return 1

    body = text.split(START, 1)[1].split(END, 1)[0]
    # The heredoc is indented ten spaces inside the YAML.
    header = "\n".join(
        line[10:] if line.startswith(" " * 10) else line for line in body.split("\n")
    )
    header = header.replace("OWNER", "example").replace("TAG", "v0.0.0")

    lines = header.split("\n")
    fences = [n for n, line in enumerate(lines) if line.strip().startswith("```")]

    problems = []
    if len(fences) % 2:
        problems.append(f"{len(fences)} code fences — unbalanced, so one block never closes")

    for opened, closed in zip(fences[0::2], fences[1::2]):
        if not [line for line in lines[opened + 1 : closed] if line.strip()]:
            problems.append(f"empty code block at lines {opened + 1}-{closed + 1}")

    if problems:
        print("release notes would render wrong:")
        for problem in problems:
            print(f"  {problem}")
        return 1

    print(f"release notes: {len(fences) // 2} code blocks, all balanced and non-empty")
    return 0


if __name__ == "__main__":
    sys.exit(main())
