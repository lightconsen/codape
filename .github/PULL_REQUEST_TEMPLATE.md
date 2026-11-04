## What changed

## Why

## Tests

- [ ] `cargo test --workspace --locked`
- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --locked` (no new warnings in the touched files)
- [ ] `cargo check --workspace --all-features --locked` (if an optional feature is touched)

## Notes

- [ ] This touches `.github/workflows/` (the pusher needs the `workflow` token
      scope)
- [ ] Documentation updated for the behaviour change
