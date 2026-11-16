---
name: security-review
description: Review pending changes for security defects — injection, path traversal, secret exposure, unsafe deserialization, missing authorization — before they land.
allowed_tools: [Read, Grep, Glob]
---

# Security review

This is a focused review of the change in front of you, not an audit of the
repository. Read the diff, then read enough of the surrounding code to know what
is user-controlled.

## Steps

1. **Get the diff.** `git diff`, `git diff --staged`, or `git diff <base>...HEAD`.
2. **Classify each changed input.** For every value that crosses a boundary —
   HTTP request, tool argument, file content, environment variable, MCP response,
   provider response — decide whether it is trusted. Anything the person, a
   model, a remote server or a file on disk supplies is untrusted.
3. **Trace untrusted values to a sink.** The sinks that matter: a shell command,
   a SQL string, a filesystem path, a URL, a deserializer, an HTML/terminal
   rendering surface, a log line.
4. **Report only what a concrete input can exploit.** Name the input, the sink,
   and the consequence.

## The patterns to look for

- **Command injection.** A value interpolated into a shell string instead of
  passed as an argument. Look for `format!` into `sh -c`, or a path joined into
  a command line.
- **Path traversal.** `join(user_value)` without rejecting `..`, absolute paths,
  or a symlink escape. A path check that canonicalizes *after* reading is too
  late.
- **Secret exposure.** A key or token written to a file, an argument, a log line,
  an error message, a transcript, or a remote request. Reading a secret from the
  environment is fine; passing it on a command line is not.
- **Missing authorization.** A new code path that reaches a privileged action
  without the permission check its siblings perform.
- **Unsafe deserialization.** Parsing untrusted bytes with a deserializer that
  can construct arbitrary types, or trusting a field the protocol says is
  optional.
- **Weak comparison.** Secret or signature comparison that short-circuits on the
  first differing byte.

## Do not report

- A theoretical weakness with no reachable input on a path the change touches.
- The absence of a defense the project explicitly documents as out of scope —
  check the docs before flagging something the codebase has decided against.
- General advice ("validate your inputs") with no line attached.

## Output

Each finding gets the file and line, the untrusted input, the sink, and what an
attacker gains. Rank by exploitability. If nothing exploitable survives
verification, say so plainly rather than padding the list.
