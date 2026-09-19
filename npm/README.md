# tilth

Smart code reading for humans and AI agents. tilth parses your code with tree-sitter and answers questions that text search can't. It shows where a symbol is defined, who calls it, what it calls, what depends on a file, and what changed at function level.

This package downloads the prebuilt tilth binary for your platform and runs it.

```bash
npx tilth <path>                  # read a file, as an outline if it is large
npx tilth <symbol> --scope <dir>  # definitions first, then usages
npx tilth <symbol> --callers      # every call site of a symbol
npx tilth install claude-code     # add tilth to an agent host as an MCP server
```

The full documentation is in the [GitHub README](https://github.com/jahala/tilth#readme), and there is an overview at [jahala.github.io/tilth](https://jahala.github.io/tilth/).

MIT licensed.
