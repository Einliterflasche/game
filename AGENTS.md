# AGENTS.md

This file provides guidance to agents such as Claude Code when working with code in this repository.

# Project Overview

// Todo

# Repository Structure

The Rust/Bevy game code lives in the `code/` subdirectory. 
This repo will at some point also contain other contents, like game design docs,
textures, etc.

# Code part

Rust game project using **Bevy 0.18.1** (edition 2024).

## Build & Run Commands

- **Build:** `cd code && cargo build`
- **Run:** `cd code && cargo run`
- **Check (fast compile check):** `cd code && cargo check`

After making code changes, always kill existing game processes and relaunch the game so the user can test immediately.


## Always read the Bevy source code

This repo contains the bevy sourcecode in the `code/bevy/` directory.
It is a copy of the bevy repo, checked out to the exact version this project is using.
Whenever there is could be any reason to check the bevy codebase, do it.
Do it using that folder.
It is trusted.

Never claim some bevy function / type / API exists without checking it exists before.
Bevy is a project with frequently updating APIs.
Whatever you _think_ you know it probably outdated/incorrect.


**Always check the examples folder to check for idiomatic patterns that could apply.**

## Code guidelines
 - Prefer early returns using `if let` / `let ... else`
 - avoid deep nesting
 - when needing to manipulate Options/Results, check for applicable functional style methods
   on them like map, map_err, ok_or, etc.
 - prefer a functional, data driven style over object orientation
 - make it idiomatic ECS code
 - After finishing writing code, look at it again with new eyes.
   Is there a possibility to apply these guidelines?
   Some pattern which could be written more concisely?

## File organization

Files should be organized from top to bottom, most important to least important items.
Files should be ordered in two sections:
 1. Types, trait definitions
 2. impls, trait impls, free standing functions

Within section 2, standard trait impls (Default, Display, Error, etc.) go near the bottom — the main logic and systems are more important.


## Dependencies

Before suggesting libraries to use with Bevy, check out the current Bevy version this project uses and make sure the suggested dependencies support this exact version.

## Git Commits

Never add a "Co-Authored-By" line to commit messages.
