# Tool descriptions for the "agent" tool profile

Each section is the description sent to the model for one tool. Section titles are the tool names. Text between
`<!-- -->` is a note for implementers and is not sent.

## Read
Reads a file from the local filesystem and returns its contents with line numbers.

- `file_path` must be an absolute path.
- By default returns up to 2000 lines from the start. For large files pass `offset` (1-based first line) and `limit` (number of lines) to read the part you need; the output tells you when more lines remain and which `offset` continues.
- Lines longer than 2000 characters are cut, with a marker.
- Output format: each line is prefixed with its line number and a tab. The prefix is not part of the file: never copy it into Edit's `old_string` or `new_string`.
- Images (png, jpg, gif, webp) are returned as images. PDFs and other binary files are reported as such.
- Reading a directory returns an error; use Glob or the shell to list directories.
- You must Read a file before you Edit or overwrite it. Reading many related files in one turn is encouraged.
<!-- maps to `read`: file_path→filePath, offset, limit -->

## Write
Creates a file, or replaces an existing file entirely.

- `file_path` must be absolute; missing parent directories are created.
- If the file already exists you must Read it first in this session; the call fails otherwise.
- Prefer Edit for changing existing files: it is safer and shows a smaller diff.
- Do not create documentation or README files unless asked.
<!-- maps to `write`: file_path→filePath, content -->

## Edit
Replaces exact text in a file.

- `file_path`: absolute path of a file you have already Read in this session.
- `old_string`: text copied exactly from the file, including indentation and line breaks, without Read's line-number prefixes. It must occur exactly once unless `replace_all` is true. Include enough surrounding lines to make it unique.
- `new_string`: the replacement; it must differ from `old_string`. Keep the file's indentation style (tabs vs spaces).
- `replace_all`: replace every occurrence (for renames).
- On failure the error says why: not found (re-Read the file; it may have changed or your copy has wrong whitespace) or found N times (add context or use `replace_all`).
- Make several small Edits rather than one huge one.
<!-- maps to `edit`: file_path→filePath, old_string→oldString, new_string→newString, replace_all→replaceAll -->

## Glob
Finds files by name pattern, newest first.

- `pattern`: glob such as `**/*.rs` or `src/**/test_*.py`.
- `path`: directory to search in (absolute); defaults to the working directory.
- Respects .gitignore. Returns at most 100 paths; narrow the pattern if the result is truncated.
- Use it instead of `find`/`ls` in the shell.
<!-- maps to `glob`: pattern, path -->

## Grep
Searches file contents with regular expressions (ripgrep syntax).

- `pattern`: regex. Escape literal braces and other special characters (`interface\{\}`).
- `path`: file or directory (absolute); defaults to the working directory.
- `glob`: restrict files, e.g. `*.ts` or `*.{ts,tsx}`.
- `output_mode`: `files_with_matches` (default: paths only), `content` (matching lines with line numbers), or `count`.
- `-i`: case-insensitive. `context`: lines of context around each match in `content` mode.
- `head_limit`: cap the number of results.
- Use it instead of grep/rg in the shell. Start with `files_with_matches` to locate, then `content` on the few files that matter.
<!-- maps to opencode_tools grep / ripgrep::grep_detailed; output_mode/context/head_limit implemented in the adapter -->

## Bash
<!-- Name is `Bash` when the session shell is bash (Git Bash on Windows), `PowerShell` for pwsh/powershell, `Cmd` for cmd.exe. The first line below is adapted to the shell. -->
Runs a command in a {shell} shell and returns its output.

- `command`: the command line. Quote paths with spaces. Chain dependent commands with `&&` (bash) or `; if ($?) { ... }` (Windows PowerShell 5.1, which has no `&&`).
- `description`: 5-10 words saying what the command does.
- `timeout`: milliseconds (default 120000, max 600000).
- `workdir`: directory to run in (absolute); prefer this over `cd`.
- `background`: `true` starts the command and returns at once with a shell ID and the file its output streams to (dev servers, long builds). No timeout unless you give one; no trailing `&` needed. You are notified with its output when it ends, so do not poll or sleep waiting for it; keep working on what does not depend on it, or end your turn.
- The shell does not keep state between calls (no persistent `cd`, variables or virtualenv activation).
- Use it for builds, tests, git, package managers and running programs. Do not use it to read, search or edit files: Read, Grep, Glob, Edit and Write are faster and safer.
- Output beyond the size limit is truncated; the full output is saved to a file whose path is given, which you can Read.
- Never run interactive commands that wait for input (editors, `git rebase -i`, prompts). Pass non-interactive flags instead.
<!-- maps to `bash`: command, timeout, workdir, description, background (Rust build: v2 `background: true`; the notice is a synthetic user message `<shell id=... state=... command=...>`, delivered at the next step boundary or by waking the idle session; background shells die with their engine or app) -->

## WebFetch
Fetches a URL and returns its content as markdown (or the format you ask for).

- `url`: full http(s) URL.
- `format`: `markdown` (default), `text` or `html`.
- Only public pages work; authenticated pages fail.
<!-- maps to `webfetch`: url, format, timeout -->

## TodoWrite
Keeps the task list for this session. Send the whole list each time.

- Each item: `content` (imperative, e.g. "Fix login redirect"), `status` (`pending`, `in_progress`, `completed`, `cancelled`), `priority` (`high`, `medium`, `low`).
- Exactly one item `in_progress` while you work. Mark items `completed` immediately when done; do not batch.
- Use it for tasks with three or more steps or when the user lists several things. Skip it for single quick actions.
<!-- maps to `todowrite`: todos[{content,status,priority}], ids generated when absent -->

## Task
Starts a subagent that works independently and returns a final report.

- `description`: 3-5 words.
- `prompt`: a complete brief. The subagent sees nothing of this conversation: state the goal, what you already know, where to look, and what to return.
- `subagent_type`: one of the available agents (listed below the tool description).
- Use it for broad searches or self-contained sub-tasks; run several in one turn when they are independent. Do not use it for a single file read or a known-location lookup.
<!-- maps to `task`: description, prompt, subagent_type -->
