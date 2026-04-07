# AGENTS.md

This file provides guidance to agents such as Claude Code when working with code in this repository.

# Project Overview

First-person fantasy spell-dueling game. Long term: MMO-scale PvP. Currently a
server-authoritative multiplayer MVP. Design docs live in design/.

# Repository Structure

code/ is a Cargo workspace with three crates:

- shared: simulation types and systems, headless-compilable
- server: headless authoritative binary
- client: graphical binary with rendering and input

code/bevy/ is a checked-in copy of the Bevy source for reference only, not a
workspace member.

# Code part

Rust game project using Bevy 0.18.1 (edition 2024).

## Build & Run Commands

Run from code/. The Justfile has shortcuts.

- just server: run the headless server
- just client: run a client (optionally pass host:port for a remote server)

For multiplayer testing, run the server in one terminal and clients in others.
Clients default to 127.0.0.1:5000.

After changes, kill running processes and relaunch. Changes in shared/ require
restarting both sides.

## Multiplayer architecture

The server runs authoritative simulation and physics. Clients render replicated
state and forward inputs each tick. The sim/render split is enforced by the
crate boundary: only the server loads physics and sim systems.

No client-side prediction or interpolation yet, so the local player sees
themselves at the latest replicated position. Next step is local prediction.


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
