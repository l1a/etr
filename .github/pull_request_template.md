## Description

Please include a summary of the change and which issue is fixed. Please also include relevant motivation and context.

Fixes # (issue)

## Type of change

- [ ] Bug fix (non-breaking change which fixes an issue)
- [ ] New feature (non-breaking change which adds functionality)
- [ ] Breaking change (fix or feature that would cause existing functionality to not work as expected)
- [ ] Documentation / tests / chore

## How Has This Been Tested?

Please describe the tests that you ran to verify your changes.

- [ ] `just check` passes (fmt + clippy)
- [ ] `just test` passes
- [ ] End-to-end tested with `just e2e-local` (if applicable)

## Checklist

- [ ] Version bumped in `Cargo.toml` (patch for fixes/docs/tests, minor for features)
- [ ] `just man` run and succeeds
- [ ] New public functions have `///` doc comments
- [ ] New or changed CLI flags updated in `--help` text and man page
- [ ] `NOTES.md` updated to reflect architecture/state changes
- [ ] Wiki updated (if user-visible behaviour changed)
