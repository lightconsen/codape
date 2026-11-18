---
name: debug
description: Find the cause of a bug before changing anything — reproduce it, shrink it, prove the mechanism, then fix that and only that.
allowed_tools: [Read, Grep, Glob, Bash, Edit]
---

# Debug

A bug is a claim about the world that is false. The work is finding *which*
claim, because the fix is obvious once you know and guesswork until you do.
Changing code before you can explain the failure is how one bug becomes two.

## Steps

1. **Reproduce it on demand.** A bug you cannot trigger is a bug you cannot
   verify a fix for. Find the shortest command, input, or sequence that fails
   every time. If it only fails intermittently, that *is* the bug — go to
   "Intermittent" below before touching anything.
2. **Watch it fail, and read the whole output.** The panic message, the stack
   frame in *your* code, the actual vs. expected values, the exit status, the
   log line just before the silence. Do not skim to the first line that looks
   familiar.
3. **Shrink the reproducer.** Remove inputs, arguments, and setup steps one at
   a time, keeping it failing. What remains is the part that matters, and it is
   usually smaller than you expect.
4. **Form one hypothesis and test it.** "The cache returns the stale entry
   because the key omits the variant." Say what you expect to see if it is
   true, then check. A hypothesis that only says "something is wrong here" is
   not a hypothesis.
5. **Prove the mechanism.** Instruments before edits: a log line, a breakpoint,
   a deliberately failing assertion, `dbg!`. You want to see the value at the
   moment it is wrong. Delete the instrumentation before you finish.
6. **Fix the cause, then the neighbours.** Ask where else the same mistake
   could be — grep for the pattern, not just the line. Then re-run the
   reproducer and the nearest tests.

## Bugs that look like something else

- **It worked yesterday.** `git log` and `git diff` over the files on the
  failing path. A dependency bump or a config change counts as a code change.
- **Works locally, fails in CI.** Environment, ordering, parallelism, clock,
  locale, file permissions. Reproduce with the same settings before theorising.
- **Works in isolation, fails in the suite.** Shared state: a temp path reused,
  an environment variable set and not reset, a global initialised once.
- **It stopped failing after you changed something unrelated.** That is a
  timing change, not a fix. Find out why it moved.

## Intermittent failures

Treat the nondeterminism as the subject. What varies — thread order, hash
iteration order, wall-clock time, a network call, a race between a write and a
read? Make it deterministic (inject the clock, sort the map, sequence the
steps), then confirm the failure turns into a consistent one. A retry loop or a
sleep is not a diagnosis.

## Output

Name the mechanism in two or three sentences: what the code did, what was
expected, and why the difference shows up on that input. Then the fix, and what
you ran to show it is gone. If you could not prove the cause, say so and say
what you ruled out — a wrong confident explanation costs the next reader more
than an honest partial one.
