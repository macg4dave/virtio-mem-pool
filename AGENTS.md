# Repository Agent Instructions

Project documentation starts at [docs/](docs/).
Task prompts are in `.github/prompts/`.

## Operate through the project's own tooling

The project is now a product, not an early prototype. Anything done to a
virtual machine, a guest, the host or the controller goes through the
project's own tools: the `rampire` command, `cargo xtask` and the scripts
under `tools/`. That covers creating, adding, configuring, deploying to,
starting, stopping, resizing, observing and testing VMs.

- Use the existing tool when one does the job. Check `rampire --help`,
  `cargo xtask --help`, [Tools](docs/tools.md) and
  [Administration](docs/administration.md) first.
- If no tool does it, add one (a `rampire` subcommand, an `xtask` command or
  an option on an existing one), document it, then use it. Do not do the job
  by hand in the meantime.
- If a task takes several tool invocations that always go together, combine
  them into one command or option and update the docs.
- Do not write one-off scripts, ad hoc `ssh`/`virsh`/`qemu` command chains
  or throwaway helpers to operate the product, in the repository or in a
  scratch directory. Work done once by hand is work the next user cannot
  repeat.
- If a tool is broken or gives the wrong result, fix the tool rather than
  working around it.

This rule is about operating the product. It does not restrict editing,
building or testing the code in this repository.
