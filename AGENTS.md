# AI Agent Guidelines (AGENTS.md)

Welcome! This file contains project-specific guidelines, constraints, and instructions for all AI assistants (Gemini, Claude, etc.) contributing to the **etr** project.

---

## 1. Project Overview
`etr` is a Rust implementation of the C++ tool **Eternal Terminal (et)**. It is a remote shell that automatically reconnects without interrupting the session.

---

## 2. Core Developer Guidelines
* **Safety First:** Avoid `unsafe` Rust unless absolutely necessary for low-level system integrations (like PTY allocation). If `unsafe` is used, it must be thoroughly documented with safety comments.
* **Idiomatic Rust:** Follow standard Rust styling, formatting (`rustfmt`), and linting (`clippy`). Prefer standard library constructs and robust, well-established crates (e.g., `tokio` for async, `clap` for CLI parsing).
* **Architecture:** The design should support a client-server architecture similar to the original Eternal Terminal.
* **Testing & Documentation Mandates:**
  * When writing code, always write the corresponding tests to go with it.
  * Always document the code clearly as you go.
  * All new features must include unit or integration tests where applicable.

---

## 3. Workflow & Source Control
* **Branching:** Always create a new feature/fix branch before making modifications. Use the prefix pattern `{feature,fix,chore,etc.}/<branch-name>`.
* **Commits:**
  * Keep commit subject lines under 50 characters, in the imperative mood.
  * Always append the AI attribution line as a trailer: `Assisted-By: <Model Name>` (e.g., `Assisted-By: Gemini 3.5 Flash`).
* **PRs & Merges:** Never push to `main` directly or run background push/commit operations without explicit user confirmation.
