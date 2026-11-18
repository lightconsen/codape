---
name: simplify
description: Make a diff smaller and easier to read without changing what it does — remove duplication, unused paths, and needless indirection, and keep the behaviour identical.
allowed_tools: [Read, Grep, Glob, Edit]
---

# Simplify

The target is a change that does the same thing in less code. Behaviour is
fixed; everything else is negotiable. If a simplification would alter what the
code does — including its error messages, its ordering, or its side effects —
it is a behaviour change, and it belongs in its own change with its own
justification.

## Steps

1. **Fix the behaviour baseline first.** Note what the code does today: run the
   tests that cover it before you touch it, so you can tell a simplification
   from a regression.
2. **Read the whole change, then the code around it.** Simplification applies
   to what the change introduced, not to the file it landed in. Unrelated
   cleanup makes the diff harder to review and belongs somewhere else.
3. **Ask these in order.** Each answer that is "yes" is a finding:
   - Is there a second copy of this logic? Could both call one function?
   - Is this abstraction used once? Inline it.
   - Is this parameter, field, or branch ever taken? Remove it — the compiler
     is the witness.
   - Is this recomputing something already available a line above?
   - Is this error path duplicating a case an earlier guard already handled?
   - Could the standard library or an existing project helper do this?
4. **Prefer the smaller edit.** A simplification that is itself hard to read
   has moved the cost, not removed it. If the reviewer has to work out what the
   new shape means, keep the old one.
5. **Re-run the same tests.** Same results, less code, or it is not a
   simplification.

## What is not a simplification

- Renaming things the project already settled on.
- Reformatting, reordering imports, or moving code between modules for taste.
- Changing a public signature, even to something tidier — that is a
  compatibility decision.
- Deleting a check because "it can't happen". Say why it cannot, with the
  invariant that guarantees it, or leave it.

## Output

List what you removed or merged and why it was safe, one line each. If the
change was already about as small as its job allows, say that in one sentence
rather than forcing an edit.
