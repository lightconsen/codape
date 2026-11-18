---
name: explain
description: Explain how a piece of code works — trace a call path, a data flow, or a subsystem — from the entry point down, at the level of detail the question needs.
allowed_tools: [Read, Grep, Glob]
---

# Explain

An explanation answers a question someone actually has. "How does this work?"
usually means "where does this value come from", "why is this check here", or
"what happens when I call this". Find which one before writing paragraphs
nobody asked for.

## Steps

1. **Establish the question.** If it was not stated, infer it from where the
   person is looking, and say at the top what you are answering. "You asked
   what happens when a tool call is denied — here is that path."
2. **Find the entry point.** The public function, the request handler, the
   event arm. Everything below it is the answer; everything beside it is not.
3. **Follow one thread all the way down.** A call path or a data flow, in
   order, naming each function and what it contributes. Resist the urge to
   describe every branch — pick the main line and mention the other branches as
   they become relevant.
4. **Stop at the level asked for.** "What does this function do" wants a
   paragraph and the one subtlety. "How does the whole subsystem fit together"
   wants the components, their boundaries, and the invariants each one
   guarantees.
5. **Name what is surprising.** The trick, the workaround, the invariant that
   is not enforced by the type system, the ordering that matters. That is the
   part a reader cannot get from skimming, and the part worth the most.

## Shape

For a call path:

```
`run()` (foo.rs:120) reads the pending call and dispatches on `name`.
  → `execute()` (bar.rs:44) checks the permission mode and, on Allow,
    hands the input to `spawn()`.
    → `spawn()` (bar.rs:210) writes it to the sandbox's stdin and reads
      one line back. It blocks; there is no timeout on this read.
```

For a data flow, trace the value rather than the functions: where it is
created, each place it is transformed, and where it is finally consumed.

For a subsystem, start with the components and one sentence each, then the
seams between them and the order they run in.

## Do not

- Narrate the file. "This module has an `Index`, a `Cache`, and a `Loader`" is
  a directory listing, not an explanation.
- Write up everything you read. If a function turned out not to be on the path,
  leave it out.
- Hedge. If you traced it, say what it does. Uncertainty belongs to the one
  part you could not follow, named explicitly.
- Quote large blocks of code. A citation like `foo.rs:120` is enough; paste only
  the line the explanation turns on.

## Output

The answer, in the order a reader needs it: what happens, where, and what is
worth knowing. Cite `file:line` so they can check you.
