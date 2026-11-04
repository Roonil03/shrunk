<!--
Thanks for contributing to shrunk! Please fill in this template and remove any
sections that do not apply. Keep the PR focused on one logical change.
-->

## Summary

<!-- One or two sentences: what does this PR do and why? -->

## Type of change

<!-- Put an `x` in the boxes that apply. -->

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds functionality)
- [ ] Breaking change (fix or feature that changes existing behavior)
- [ ] Performance improvement
- [ ] Documentation update
- [ ] Refactor or internal cleanup

## Motivation and context

<!--
Why is this change needed? Link the issue or discussion if one exists.
What problem does it solve, and what alternatives were considered?
-->

Closes #

## What changed

<!-- A short bullet list of the important changes. Mention format or API changes. -->

-
-

## How was this tested?

<!--
List the exact commands you ran and any new tests or fixtures you added.
-->

```sh
cargo fmt --check
cargo test --all-targets
cargo test --doc
```

## Benchmarks (if applicable)

<!--
For performance or compression changes, include before/after numbers and the
command used to reproduce them. Note the hardware and build profile.
-->

## Checklist

- [ ] I ran `cargo fmt --check`
- [ ] I ran `cargo test --all-targets`
- [ ] I ran `cargo test --doc`
- [ ] I added or updated tests for this change
- [ ] I updated the documentation (README, docs, doc comments)
- [ ] Existing on-disk files remain readable, or a migration path is included
- [ ] Public API changes are documented and justified
- [ ] No new runtime dependencies were added without discussion

## Screenshots, logs, or additional notes

<!-- Optional. -->
