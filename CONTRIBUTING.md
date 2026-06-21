# Contributing to etr

First off, thank you for considering contributing to `etr`! It's people like you who make the open-source community such an amazing place to learn, inspire, and create.

## How Can I Contribute?

### Reporting Bugs

- **Check for existing issues:** Before opening a new issue, please search the issue tracker to see if the problem has already been reported.
- **Provide reproduction steps:** A clear description of how to reproduce the bug helps us fix it faster.
- **Include environment details:** OS, shell, `etr` version (`etr --version`), and how you're connecting (localhost, remote, SSH port, etc.).

### Suggesting Enhancements

- **Open a GitHub issue** describing your idea and why it would be useful.
- **Be specific:** Describe the desired behavior and any potential edge cases.

### Submitting Pull Requests

1. **Fork the repository** and create your branch from `main` using the prefix pattern `{feature,fix,chore,docs}/<branch-name>`.
2. **Ensure the code follows the project's style:** We use `rustfmt` and `clippy`.
3. **Run the quality gate:** `just check` (fmt + clippy) and `just test` must both pass.
4. **Write tests:** Every new public function and every bug fix needs a corresponding test.
5. **Bump the version** in `Cargo.toml` (patch for fixes/docs/tests, minor for new features) and run `just man`.
6. **Write a clear commit message:** Imperative mood, subject line under 50 characters.
7. **Link related issues:** Mention any related issues in your PR description.

## Development Setup

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (latest stable)
- [just](https://github.com/casey/just) (command runner)
- [pandoc](https://pandoc.org/installing.html) (for generating man pages)
- SSH access to localhost (required for the end-to-end test suite)

### Build and Run

```bash
git clone https://github.com/l1a/etr.git
cd etr
cargo build
```

### Checking your changes

Before submitting a PR, please run:

```bash
just check   # cargo fmt --check + cargo clippy -D warnings
just test    # all unit and integration tests
```

### End-to-end tests (optional but encouraged)

```bash
just check-tools   # verifies tmux, ssh, passwordless localhost SSH
just e2e-local     # full happy-path + reconnect test against localhost
```

## Licensing

By contributing to `etr`, you agree that your contributions will be licensed under the project's **GPL-3.0 License**.
