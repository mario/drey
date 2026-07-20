**What this changes**

<!-- The behaviour, not the diff. -->

**What breaks if it is wrong**

<!-- The most useful section. If a sharing decision goes the wrong way here,
     what does the user see, and how loudly? -->

**Checks**

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test`
- [ ] `cargo build && python3 tests/e2e.py`
- [ ] A test fails if this change is reverted
- [ ] Docs updated if behaviour or config changed
