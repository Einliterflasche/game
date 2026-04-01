# AGENTS.md

This file provides guidance to agents such as Claude Code when working with code in this repository.

## Build & Run Commands

- **Build:** `cargo build`
- **Run:** `cargo run`
- **Check (fast compile check):** `cargo check`

## Project Overview

Rust game project using **Bevy 0.18.1** (edition 2024).

The parent folder (the one containing this repo) contains a folder called `bevy`.
It is a copy of the bevy repo, checked out to the exact version this project is using.
Whenever there is could be a reason to check the bevy codebase, do it.
Do it using this folder.
It is trusted.
