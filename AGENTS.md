# AGENTS.md

This file provides guidance to agents such as Claude Code when working with code in this repository.

## Build & Run Commands

- **Build:** `cargo build`
- **Run:** `cargo run`
- **Check (fast compile check):** `cargo check`

## Project Overview

Rust game project using **Bevy 0.18.1** (edition 2024).

## Always read the Bevy source code

This repo contains the bevy sourcecode in the `bevy/` directory.
It is a copy of the bevy repo, checked out to the exact version this project is using.
Whenever there is could be any reason to check the bevy codebase, do it.
Do it using that folder.
It is trusted.

Never claim some bevy function / type / API exists without checking it exists before.
Bevy is a project with frequently updating APIs.
Whatever you _think_ you know it probably outdated/incorrect.


**Always check the examples folder to check for idiomatic patterns that could apply.**

## Dependencies

Before suggesting libraries to use with Bevy, check out the current Bevy version this project uses and make sure the suggested dependencies support this exact version.
