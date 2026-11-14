# Test fixtures

Settings files that unit tests load. They describe no real site: change them
only together with the tests that read them. Site configuration lives
outside the repository; [deploy/examples](../../deploy/examples) holds
the documented examples.

This folder is a configuration directory with one VM, `windows-guest`.
`linux-guest.toml` sits outside `vms.d/` so that tests can load it on its
own without adding a second VM to the configuration.
