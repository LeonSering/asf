# asf - Agent Session Finder

Find, search and resume your past [Claude Code](https://docs.claude.com/en/docs/claude-code) conversations, in a terminal UI.

Claude Code saves every conversation in `~/.claude/projects/`, but finding an old one again means remembering its folder or scrolling through the `/resume` picker. asf turns that history into a searchable list across all your projects and drops you back into the right folder with the conversation resumed.

asf works on the saved conversation history. It doesn't run or manage live agents.

![asf demo](demo/asf.gif)

> **Written by AI.** The code of this tool was written by an AI ([Claude Code](https://docs.claude.com/en/docs/claude-code)), based on my instructions and feedback. Use it at your own risk, especially the features that change session files (rename, move, delete).

## Features

- **All sessions in one list**, newest first, or as a folder tree (`Tab`). Paths under your home are shown as `~`.
- **Full-text search** over your questions and the agent's replies (tool output is skipped). Results are ranked: name or folder matches first, then by number of matches, then by date. The tree view only filters.
- **Preview pane** with all your questions and the end of the conversation. While searching, it shows the conversation around each match and lets you jump between matches.
- **No accidental double resume**: conversations that are currently open in a running Claude Code are marked (`●` busy, `○` idle), and asf asks before resuming one a second time.
- **Session details**: number of questions and lines, and the git branch the session worked on.
- **Rename, move and delete** sessions. Moving rewrites the session to belong to another folder.
- **Ignore file** with gitignore-style folder patterns to hide sessions you never want to see.

## Install

### Requirements

- [Claude Code](https://docs.claude.com/en/docs/claude-code), with `claude` on your `PATH`.
- Rust 1.88 or newer, installed via [rustup](https://rustup.rs).
- A terminal with true color and a font that has symbols like `▼ ● ○ ⎇`. Most modern terminals and fonts are fine.

### Build and install

```sh
git clone https://github.com/LeonSering/asf
cd asf
cargo install --locked --path .
```

This puts the `asf` binary into `~/.cargo/bin`, which rustup adds to your `PATH`.

### Shell integration (recommended)

A program can't change the directory of the shell that started it. To end up in the session's folder after Claude Code exits, source the wrapper function from your `~/.zshrc` or `~/.bashrc`:

```sh
source /path/to/asf/shell/asf.sh
```

Open a new terminal (or `source` your shell config) afterwards. Without the wrapper, asf still works: it starts the command directly in the session folder, but your shell stays where it was afterwards.

### First start

The first start reads every session file once to build the search index. The title bar shows `indexing N/M` until it's done; you can use the list right away. Later starts only read what changed.

### Keep old sessions

By default, Claude Code deletes session transcripts after 30 days. To keep them longer, set `cleanupPeriodDays` in `~/.claude/settings.json`:

```json
{
  "cleanupPeriodDays": 365
}
```

## Usage

Run `asf`, type to search, and press `Enter` to resume the selected session.

| Key | Action |
|---|---|
| typing | search (name, folder and conversation text) |
| `↑` `↓`, `Ctrl-k` `Ctrl-j` | move selection (`k` `j` work while the search is empty) |
| `PageUp` `PageDown`, `Home` `End` | move by page, jump to first or last |
| `Enter`, `Alt-a` | open the selected session (or fold a folder in the tree) |
| `Alt-v` | open with the alternative command, if configured |
| `Tab` | switch between list and folder tree |
| `←` `→` | collapse or expand folders (tree view) |
| `Ctrl-n` `Ctrl-p` | next or previous search match in the preview |
| `Ctrl-↑` `Ctrl-↓`, `Alt-PageUp` `Alt-PageDown` | scroll the preview |
| `Ctrl-r` | rename the session |
| `Alt-m` | move the session to another folder (`Tab` completes paths) |
| `Ctrl-d` | delete the session (asks first) |
| `Alt-h` | show or hide ignored sessions |
| `Esc` | quit |

## Configuration

### Open commands: `~/.config/asf/config`

By default, `Enter` runs `claude --resume <id>` in the session folder. You can change that and add a second command for `Alt-v`:

```ini
# {id} is replaced by the session id, {cwd} by the session folder (both quoted).
open = claude --resume {id}
open_label = open

alt_open = claude --model opus --resume {id}
alt_label = opus
```

The command runs through your shell, so `~`, environment variables and aliases defined in your shell config (with the wrapper) work.

### Ignore file: `~/.config/asf/ignore`

One pattern per line, similar to `.gitignore`. Ignored sessions are hidden and not indexed until you press `Alt-h`.

```gitignore
# A folder and everything below it
~/tmp
/tmp/**

# * stays within one folder name, ** spans any depth
~/projects/*/scratch

# A bare name matches a folder of that name anywhere
node_modules

# ! brings something back, the last matching line wins
!~/tmp/keep-this

# Single sessions, by id or by name
id:015a5e47-e89e-466a-8c05-85cdcbfaec27
name:test*
```

## Files

- Reads `~/.claude/projects/` (session transcripts) and `~/.claude/sessions/` (running processes).
- Writes its cache and search index to `~/.cache/asf/`. It's safe to delete; asf rebuilds it on the next start.
- Renaming appends a rename entry to the session file. Moving rewrites the session file into the target folder's project directory. Deleting removes the session file permanently.

## Caveats

- asf relies on Claude Code's file layout and session format, which are internal and undocumented. A Claude Code update can break parts of it. Live status detection fails quietly (no markers) if its format changes.
- Developed and tested on Linux. On macOS, live status (which uses `/proc`) is not available, and the config files live in `~/Library/Application Support/asf/` instead of `~/.config/asf/`. The rest should work but is untested there.
- Not affiliated with or endorsed by Anthropic.

## License

[MIT](LICENSE)
