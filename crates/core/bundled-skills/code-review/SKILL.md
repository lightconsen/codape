---
name: code-review
description: Review a diff for real defects — wrong logic, broken edge cases, missing error paths — and report only findings you can point at a line for.
allowed_tools: [Read, Grep, Glob, Bash]
---

# Code review

Review the change, not the file. A finding is a claim that some input makes the
code do the wrong thing, and it is only worth reporting if you can name the
input and the wrong thing.

## Steps

1. **Establish the diff.** `git diff` for unstaged work, `git diff --staged` when
   the change is staged, `git diff <base>...HEAD` when asked about a branch.
   Read the surrounding function, not just the changed lines: a hunk is only
   correct in the context it lands in.
2. **Read what the change depends on.** Grep the call sites of anything whose
   behaviour, signature, or return value changed. A rename that compiles can
   still leave a caller comparing the old value.
3. **Hunt in this order.** Bugs first, then resource handling, then
   simplification. A simplification that is merely a preference is not a
   finding; leave it out unless the user asked for cleanup.
4. **Verify before reporting.** For each candidate finding, name the concrete
   input or state that triggers it and the wrong result that follows. If you
   cannot, drop it — an unverifiable warning costs the reader more than it saves.

## What counts as a bug

- Wrong behaviour on an edge case: empty input, one element, a missing file, a
  zero-length line, a negative number, an off-by-one at a boundary.
- An error path that loses information or reports success for a failure.
- A change that is correct in isolation and wrong in context — for example a
  new early return that skips cleanup an existing path performs.
- Concurrency and ordering: a lock held across an await, a check that assumes
  the value it just read is still there.

## What does not

- Style the formatter would fix, or naming the project already settled on.
- Missing tests, unless the user asked for test coverage specifically.
- Speculation about code the diff does not touch.

## Output

Most severe first. For each: the file and line, one sentence on the defect, and
the input or state that triggers it. Stop after you have reported what you
verified; do not pad the list. If the change is sound, say so in one line.
