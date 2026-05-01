# Turin

Turin is a guided tour format and player for codebases.

It lets you turn code walkthroughs into a checked-in `.turin/` directory: an ordered set of stops where each stop points at a source location and includes markdown commentary. Readers can then step through the tour in the terminal with `turin play` or from their editor.

## Why

Most useful codebase knowledge is contextual: why a module exists, where a change starts, what tradeoff a function is protecting, or which files matter after a large refactor. Turin gives that context a small, portable format.

A common workflow is to ask an LLM to create a tour for:

- a new-to-you codebase
- the architecture of a subsystem
- a pull request or recent set of changes
- a debugging path through related files
- onboarding notes that should stay close to the code

The LLM writes `.turin/tour.json` plus markdown stop files. You then play the tour and move stop by stop through the relevant code, with the explanation beside it.

## How It Works

A tour lives at the project root:

```text
<project-root>/
└── .turin/
    ├── tour.json
    ├── entry.md
    ├── dispatch.md
    └── buffer.md
```

`tour.json` contains tour metadata and the ordered stop list:

```json
{
  "tour": {
    "title": "Lexer architecture",
    "description": "How the hand-written lexer feeds the streaming parser."
  },
  "stops": [
    "entry.md",
    "dispatch.md",
    "buffer.md"
  ]
}
```

Each stop is a markdown file with TOML frontmatter:

```markdown
---
id = "entry"
file = "src/parser/lexer.rs"
anchor = { kind = "pattern", value = "fn tokenize" }
title = "Entry point"
highlight = { lines = 8 }
---

The lexer is hand-written rather than generated.
This entry point matters because it returns a stream of tokens consumed by the parser.
```

Turin resolves the anchor, opens the matching source file, highlights the relevant lines, and shows the commentary.

## LLM Workflow

Ask your coding agent to create a Turin tour for the thing you want to understand:

```text
Create a Turin tour explaining the recent parser changes.
Focus on the entry point, the token dispatch path, and the error recovery logic.
Use pattern anchors where possible.
```

Or for onboarding:

```text
Create a Turin tour for this repository.
Show me the main execution path, where configuration is loaded, and where user-facing output is rendered.
```

The useful output is not a prose document outside the repo. It is a `.turin/` directory that can be reviewed, edited, committed, and replayed. After the LLM creates or updates the tour, run:

```sh
turin list
turin play
```

`turin play` opens an interactive terminal UI with source code on the left and the stop commentary on the right. Use `n` or `]` for next, `p` or `[` for previous, `j`/`k` to scroll, and `q` to quit.

## CLI

Create a tour:

```sh
turin new --tour-title "Parser walkthrough"
```

Add a stop:

```sh
turin add \
  --title "Token dispatch" \
  --file src/parser/lexer.rs \
  --anchor-kind pattern \
  --anchor "match self.peek" \
  --highlight-lines 8 \
  --body "This branch is where the lexer turns the next character into a token."
```

Inspect the tour:

```sh
turin list
```

Play it:

```sh
turin play
```

Print the format reference:

```sh
turin quickstart
```

Use `--project-root <path>` with any command to point Turin at a different project root.

## Anchors

Stops can locate code in three ways:

- `line`: direct line number, useful for quick drafts but brittle.
- `pattern`: first regex match in the file, a good default for most tours.
- `treesitter`: tree-sitter query for editor integrations that can resolve syntax-aware anchors.

Prefer `pattern` anchors for LLM-authored tours unless a line number is all you have.

## Editors

- [turin.nvim](https://github.com/afsharalex/turin.nvim)
- [turin-mode](https://github.com/afsharalex/turin-mode)
- [turin-vscode](https://github.com/afsharalex/turin-vscode)
