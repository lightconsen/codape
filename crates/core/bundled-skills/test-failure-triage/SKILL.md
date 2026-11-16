---
name: test-failure-triage
description: Take a failing test from red to a real diagnosis — reproduce it, decide whether the test or the code is wrong, and fix the cause rather than the symptom.
allowed_tools: [Read, Grep, Glob, Bash, Edit, Write]
---

# Test failure triage

A failing test is evidence. The job is to find out what it is evidence *of*
before changing anything, because the fastest way to a green suite is to edit
the assertion, and that is usually the wrong fix.

## Steps

1. **Reproduce it alone.** Run the single test, not the suite:
   `cargo test <name> -- --nocapture`, `pytest path::test`, `go test -run`,
   `npm test -- -t "<name>"`. A test that only fails in the full suite is a
   different problem — note that and treat isolation as the subject.
2. **Read the failure, not the exit code.** Take the actual assertion message,
   the expected and actual values, and the stack frame in *your* code. If the
   failure is a timeout or a panic in a dependency, find what your code did to
   reach it.
3. **Decide which side is wrong.** Ask: what behaviour did the test intend to
   pin? If the code genuinely changed and the test encodes the old behaviour,
   the test is stale — update it and say so. If the test encodes a real
   contract, the code has a bug — fix the code. Never rewrite an assertion to
   match observed output without this decision.
4. **Check the neighbours.** Grep for other tests of the same behaviour. A
   change that fixes one and breaks three moved the problem.
5. **Re-run the failing test, then the narrowest suite around it.** Both green
   before you report.

## Common shapes

- **Flaky by construction**: a test that depends on wall-clock time, on the
  order of a hash map, on a fixed port, or on a thread finishing "soon". Fix the
  test's determinism; do not add a sleep and call it fixed.
- **Leaks between tests**: shared global state, a temp directory reused, an
  environment variable set by another test. Find the unreset state.
- **Assertion on a message**: the behaviour is right and the wording changed.
  Confirm the wording is not itself the contract before relaxing it.

## Output

One paragraph: what failed, why, and what you changed — test or code, and which
one was wrong and how you know. Leave the suite green.
