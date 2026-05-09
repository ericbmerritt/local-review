# ggr

**Local terminal review surface for GitHub pull requests.**

`ggr` opens a GitHub PR in your terminal: description and general comments
first, then walk each commit's diff file-by-file. No browser, no context switch.

## Install

```
cargo install ggr
```

Or via Homebrew:

```
brew install ericbmerritt/jjr/ggr
```

## Requirements

[`gh`](https://cli.github.com) authenticated to GitHub (or GitHub Enterprise).

## Usage

```
# Auto-detect repo from the current directory's git remote
ggr 42

# Explicit repo — works from any directory
ggr acme/myrepo#2429

# GitHub Enterprise Server
ggr --url https://github.example.com acme/myrepo#2429

# Paste a full pull URL from the browser
ggr https://github.com/owner/repo/pull/2429
```

## Keybindings

| Key                    | Action                                            |
| ---------------------- | ------------------------------------------------- |
| `↑` `↓` / `j` `k`      | scroll line                                       |
| `PgUp` `PgDn`          | scroll page                                       |
| `Home` `g` / `End` `G` | top / bottom                                      |
| `Tab` / `Shift-Tab`    | next / previous file                              |
| `n` / `p`              | next / previous commit (or description)           |
| `\|`                   | cycle diff layout (auto / unified / side-by-side) |
| `?`                    | help                                              |
| `q`                    | quit                                              |

## License

MIT OR Apache-2.0
