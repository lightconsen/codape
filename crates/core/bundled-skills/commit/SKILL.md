---
name: commit
description: Write a commit (or a series of them) that a reviewer can read — one logical change each, message that says why, nothing unrelated swept in.
allowed_tools: [Read, Bash, Edit, Write]
---

# Commit

A commit is a claim about what changed and why, and the diff is the evidence.
Someone reading `git log` in six months, or bisecting to find which change
introduced a bug, is the audience. The message is not a summary of the diff —
that is what `git diff` is for — it is the reason the diff exists.

## Steps

1. **See what is actually staged and unstaged.** `git status`, then
   `git diff` and `git diff --staged` in full. `git add -A` is how unrelated
   work — a build artefact, a debug print, someone else's in-progress edit —
   ends up in your commit.
2. **Find the seams.** A commit should be one change someone could revert
   without breaking an unrelated one. If the working tree holds two things,
   stage them separately. A rename plus a behaviour change is two commits.
3. **Stage deliberately.** Name the paths. Read `git diff --staged` once more
   before committing — this is the last chance to catch the file you did not
   mean to include.
4. **Write the message.** Subject line: imperative mood, what the change does,
   no more than about 72 characters, no trailing period. Blank line. Then the
   body: why this change, what the old behaviour was, and anything a reviewer
   would otherwise have to reconstruct. Wrap the body at 72 columns.
5. **Include what the message needs.** A test that pins the fix belongs in the
   same commit. A changelog entry the project keeps belongs there too. Nothing
   else does.

## The message

Good:

```
fix(git): keep the branch name when the description is empty

`-b ""` was parsed as "no branch given", so the command fell back to
`main` and created the commit on the wrong branch. An empty description
is legal; only an absent flag means the default.
```

Bad:

```
fixed stuff

also updated some tests and reformatted
```

The bad one has no subject worth reading, no reason, and admits to sweeping in
unrelated work.

## Before you commit

- Tests for the touched code pass, and you know they do because you ran them.
- No secret, key, token, `.env`, or credential file is staged. Check
  `git diff --staged` for one even when you are sure.
- No debug print, commented-out block, or `TODO` you introduced.
- The message follows the repository's convention — read `git log` for the last
  twenty subjects and match their form.
- Never rewrite or amend a commit that is already pushed and shared.

## Output

The one-line summary of each commit you made, and anything you found in the
working tree that you deliberately did not commit and why.
